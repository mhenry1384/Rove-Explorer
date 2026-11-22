# Project Guidelines

## Git

- Commit and push all changes directly to `main`, not a feature branch, unless the user says otherwise.

## Release Builds

- The version number is a single integer (e.g. v5, v6, v7), shown in the app as `v<N>`. Because
  npm, Cargo and Tauri require semver, it is stored as `<N>.0.0` in the version files; only the
  major part is displayed.
- After every change, do a release build (following the rules below) without being asked, then
  start the new exe.
- Before every release build, check `git status`. If there are uncommitted changes, bump the
  version number first: increment the integer (e.g. 5.0.0 -> 6.0.0) in `package.json`,
  `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml` and the `rove-explorer` entry in
  `src-tauri/Cargo.lock`. If the working tree is clean, keep the current version.
- Do not generate an installer during a release build unless the user explicitly asks for one.
  - On Windows: build the exe only, with no MSI or NSIS installer (`pnpm tauri build --no-bundle`).
  - On macOS: build the app bundle only, with no DMG (`pnpm tauri build --bundles app`).
  - When the user asks for an installer, build with `pnpm tauri build`.
- If Rove Explorer is running from `src-tauri/target/release/`, the build can't replace the exe.
  It's always fine to close the running app, build, and start it again without asking.
- Don't hand the turn back while a release build is running. Wait for it to finish (it can take
  over ten minutes, so run it in the background if needed), then start the new exe and carry on
  with the rest of the task (e.g. committing) without checking in first.
