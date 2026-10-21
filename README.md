# Rove Explorer

A focused, dual-pane file explorer built with [Tauri](https://tauri.app) (Rust) and vanilla JavaScript.

## Features

- Two independent panes, each with its own tabs, for fast side-by-side file management
- Configurable list of favorite/pinned folders (`config/folders.json`)
- Drive listing (Windows drives / mounted volumes)
- Live directory watching — panes refresh automatically when the filesystem changes
- Rename, delete (to trash), and open files/folders
- Folder size / item-count statistics computed recursively, with cancellation support
- File type icons extracted from the OS shell (Windows) with graceful fallback
- Breadcrumb path navigation and hidden-file toggle

## Tech stack

- **Frontend**: vanilla JavaScript + CSS, bundled with [Vite](https://vitejs.dev)
- **Backend/shell**: [Tauri v2](https://tauri.app) (Rust), using the dialog and opener plugins
- **Filesystem watching**: [`notify`](https://crates.io/crates/notify)
- **Delete-to-trash**: [`trash`](https://crates.io/crates/trash)
- **Icon extraction**: [`image`](https://crates.io/crates/image)

## Prerequisites

- [Node.js](https://nodejs.org/) (for the frontend and Tauri CLI)
- [Rust](https://www.rust-lang.org/tools/install) (for the Tauri backend)
- Platform-specific Tauri prerequisites — see the [Tauri prerequisites guide](https://tauri.app/start/prerequisites/)

## Getting started

Install dependencies:

```bash
npm install
```

Run the app in development mode (opens a native window backed by a Vite dev server):

```bash
npm run tauri dev
```

Build a production bundle:

```bash
npm run tauri build
```

Other scripts:

```bash
npm run dev      # Vite dev server only (browser, no native shell)
npm run build    # Build the frontend
npm run preview  # Preview the built frontend
```

## Configuration

Bookmarks shown in the folder toolbar are defined in [`config/folders.json`](config/folders.json) as `{ "path", "name", "icon" }` entries. Paths can use `~` to refer to the user's home directory. `icon` names an SVG in `public/folder-icons/` (without the extension); `name` and `icon` are optional and default to the folder name and a guess from it. The file is bundled at build time, so rebuild after editing it.

## Project structure

```
src/                  Frontend (JS/CSS)
src-tauri/            Tauri app (Rust) — commands, window setup, icon extraction
config/folders.json   Pinned folder list
public/               Static assets (folder icons, etc.)
```
