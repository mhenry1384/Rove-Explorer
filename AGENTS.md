# Project Guidelines

## Release Builds

- Before every release build, check `git status`. If there are uncommitted changes, bump the
  version number first: increment the rightmost digit (e.g. 0.4.0 -> 0.4.1) in `package.json`,
  `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`. If the working tree is clean, keep the
  current version.
- Build with `npm run tauri build`.
- On macOS: do not build or package a DMG during a release build unless the user explicitly asks
  for one. Build the app bundle only (`npm run tauri build -- --bundles app`).
- If Rove Explorer is running from `src-tauri/target/release/`, the build can't replace the exe.
  It's always fine to close the running app, build, and start it again without asking.
