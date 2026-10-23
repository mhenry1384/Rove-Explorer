# Agent notes

## Release builds

- Every release build gets a new version number. Bump the rightmost digit (e.g. 0.4.0 → 0.4.1) in
  `package.json`, `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`.
- Build with `npm run tauri build`.
- If Rove Explorer is running from `src-tauri/target/release/`, the build can't replace the exe.
  It's always fine to close the running app, build, and start it again without asking.
