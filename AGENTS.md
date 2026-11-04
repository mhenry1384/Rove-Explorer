# Project Guidelines

## Release Builds

- Before every release build, check `git status`. If there are uncommitted changes, bump the
  version number first: increment the rightmost digit (e.g. 0.4.0 -> 0.4.1) in `package.json`,
  `package-lock.json` (both the top-level `version` and the one under `packages[""]`),
  `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`. If the working tree is clean, keep the
  current version.
- Do not generate an installer during a release build unless the user explicitly asks for one.
  - On Windows: build the exe only, with no MSI or NSIS installer (`npm run tauri build -- --no-bundle`).
  - On macOS: build the app bundle only, with no DMG (`npm run tauri build -- --bundles app`).
  - When the user asks for an installer, build with `npm run tauri build`.
- If Rove Explorer is running from `src-tauri/target/release/`, the build can't replace the exe.
  It's always fine to close the running app, build, and start it again without asking.
