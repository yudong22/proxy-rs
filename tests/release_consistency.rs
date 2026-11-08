//! The shipped version number must be one number, and it must be the same one
//! everywhere.
//!
//! # The incident this exists for
//!
//! v3.5.2's `.dmg` reported `3.5.2` in `Info.plist` while the binary inside it
//! reported `3.5.1` — the console header (`#app-version`) showed **v3.5.1** on a
//! v3.5.2 release. The bump commit `bcbce36` changed four files and missed
//! `src-tauri/Cargo.toml`, so the two numbers silently diverged. Nothing failed:
//! the installer was fine, the app started, only its own version label was a
//! lie.
//!
//! # Why this is a *text* test and not a compile-time one
//!
//! The number the user sees comes from `env!("CARGO_PKG_VERSION")` inside the
//! **`proxy-rs-gui`** crate (`src-tauri/`), not from the root `proxy-rs` crate.
//! A test that imported the GUI crate to read its version would need a full Tauri
//! toolchain — and Linux CI has no `webkit2gtk`, so the GUI crate does not build
//! there at all (it is a workspace member but not a *default* member, and CI
//! deliberately never passes `--workspace`). Reading the declaration files as
//! text is the only check that runs on every platform, including the Linux
//! runner.
//!
//! # The pair that broke
//!
//! * the **installer/package version** comes from `src-tauri/tauri.conf.json`;
//! * the **console's displayed version** comes from `src-tauri/Cargo.toml`,
//!   through `env!("CARGO_PKG_VERSION")`.
//!
//! Those two are written independently and drifted once. A release where they
//! disagree is a release whose own version label contradicts the installer.
//!
//! # How to bump a release
//!
//! The root `Cargo.toml` is the anchor. Every other declaration is asserted
//! against it, so a bump is: change `Cargo.toml`, then make the rest match. The
//! test failure names every file that was left behind — that is the whole point.

use std::path::PathBuf;

/// Repo root, from this integration test's compile-time manifest directory.
fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = repo().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{rel} must be readable ({e}); the version guard reads it to keep the released version in one place")
    })
}

/// Find the `version = "x.y.z"` line of the `[package]` table in a `Cargo.toml`.
///
/// Deliberately narrow: only the first `version =` after `[package]` counts, so a
/// dependency's `version = "1.0"` cannot be mistaken for the package's own.
fn cargo_package_version(rel: &str) -> String {
    let src = read(rel);
    let mut in_package = false;
    for (i, line) in src.lines().enumerate() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package || !line.starts_with("version") {
            continue;
        }
        if let Some(v) = line.split('"').nth(1) {
            return v.to_string();
        }
        panic!(
            "{rel}:{}: version line is not a quoted string: {line}",
            i + 1
        );
    }
    panic!("{rel} must declare `version = \"...\"` in its [package] table")
}

/// Pull a **top-level** string field out of a JSON document.
///
/// The manifests are read as text on purpose (see the module docs): adding a JSON
/// dependency to the test crate would defeat the point of a check that must run
/// everywhere. Both files put their version at two-space indent; anything deeper
/// is a nested object and must not satisfy the lookup.
fn json_top_level_string(rel: &str, key: &str) -> String {
    let src = read(rel);
    let needle = format!("\"{key}\"");
    for (i, line) in src.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent > 2 || !trimmed.starts_with(&needle) {
            continue;
        }
        if let Some(v) = trimmed.split('"').nth(3) {
            return v.to_string();
        }
        panic!(
            "{rel}:{}: {key} line is not a quoted string: {trimmed}",
            i + 1
        );
    }
    panic!("{rel} must contain a top-level \"{key}\" field")
}

/// Read the `version` of the root package entry (`packages.""`) in a v3 lockfile.
///
/// It sits at deeper indent than the top-level `version`, which is why it needs
/// its own reader: grabbing "the first line that starts with `\"version\"` at
/// depth ≤ 2" would find the wrong one of the two copies.
fn package_lock_root_entry_version(rel: &str) -> String {
    let src = read(rel);
    let mut inside_root_entry = false;
    for (i, line) in src.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.starts_with("\"\":") {
            inside_root_entry = true;
            continue;
        }
        // Leaving the block: back to the `packages` object's own depth.
        if inside_root_entry && indent <= 4 {
            inside_root_entry = false;
        }
        if inside_root_entry && trimmed.starts_with("\"version\"") {
            return trimmed
                .split('"')
                .nth(3)
                .unwrap_or_else(|| panic!("{rel}:{}: malformed version line", i + 1))
                .to_string();
        }
    }
    panic!("{rel} must have a `packages.\"\"` entry carrying a version")
}

/// Read `version` of `[[package]] name = <name>` in `Cargo.lock`.
fn cargo_lock_version(name: &str) -> String {
    let src = read("Cargo.lock");
    let wanted = format!("\"{name}\"");
    let mut for_this_package = false;
    for line in src.lines() {
        if let Some(rest) = line.strip_prefix("name = ") {
            for_this_package = rest.trim() == wanted;
            continue;
        }
        if !for_this_package {
            continue;
        }
        if let Some(rest) = line.strip_prefix("version = ") {
            return rest.trim().trim_matches('"').to_string();
        }
    }
    panic!("Cargo.lock must contain a `[[package]]` named {wanted}; run `cargo metadata` to regenerate it")
}

/// Every place the release version is declared, with what that copy controls.
///
/// Covered here so a new declaration site cannot be added without someone
/// noticing this list: an unguarded copy is exactly how 3.5.2 got two versions.
fn declared_versions() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "Cargo.toml",
            "the root `proxy-rs` crate — the headless server's own `CARGO_PKG_VERSION` \
             (`/health`, logs, `/admin` server console)",
            cargo_package_version("Cargo.toml"),
        ),
        (
            "src-tauri/Cargo.toml",
            "the `proxy-rs-gui` crate — what the DESKTOP CONSOLE HEADER shows \
             (#app-version), via `env!(\"CARGO_PKG_VERSION\")`",
            cargo_package_version("src-tauri/Cargo.toml"),
        ),
        (
            "src-tauri/tauri.conf.json",
            "the BUNDLE VERSION baked into the .dmg/.msi/.exe and Info.plist",
            json_top_level_string("src-tauri/tauri.conf.json", "version"),
        ),
        (
            "package.json",
            "the npm package version the Tauri CLI reads",
            json_top_level_string("package.json", "version"),
        ),
        (
            "package-lock.json (top level)",
            "the locked npm package metadata",
            json_top_level_string("package-lock.json", "version"),
        ),
        (
            "package-lock.json (packages.\"\" entry)",
            "the locked npm package metadata for the workspace root",
            package_lock_root_entry_version("package-lock.json"),
        ),
        (
            "Cargo.lock (proxy-rs)",
            "the locked version of the root crate; a stale entry makes `--locked` fail",
            cargo_lock_version("proxy-rs"),
        ),
        (
            "Cargo.lock (proxy-rs-gui)",
            "the locked version of the GUI crate — this is where 3.5.2 was stuck at 3.5.1",
            cargo_lock_version("proxy-rs-gui"),
        ),
    ]
}

/// All declarations must carry the same number as the root `Cargo.toml`.
///
/// A single test rather than one per file: when a bump forgets several files,
/// one failure that lists all of them is what actually gets fixed correctly.
#[test]
fn every_declared_version_is_the_same_number() {
    let declared = declared_versions();
    let (anchor_file, _, anchor) = &declared[0];

    let mismatched: Vec<String> = declared
        .iter()
        .filter(|(_, _, v)| v != anchor)
        .map(|(file, consumer, v)| format!("  {file} = {v}   ({consumer})"))
        .collect();

    assert!(
        mismatched.is_empty(),
        "release version drift: {anchor_file} says {anchor}, but these disagree with it:\n{}\n\
         Every copy must carry the same number. The root `Cargo.toml` is the anchor: \
         bump it, then make the rest match. Different copies are consumed by different \
         artifacts — the bundle version is read from tauri.conf.json while the desktop \
         console's displayed version is compiled in from src-tauri/Cargo.toml, so a \
         mismatch ships a binary that misreports itself (that is exactly what v3.5.2 \
         did: 3.5.2 installer, v3.5.1 console header).",
        mismatched.join("\n"),
    );
}

/// The pair that actually shipped the wrong label in v3.5.2, asserted on its own
/// so the failure message names the two consumers instead of blaming "the
/// version files".
#[test]
fn the_packaged_version_and_the_console_version_are_the_same_number() {
    let bundle = json_top_level_string("src-tauri/tauri.conf.json", "version");
    let console = cargo_package_version("src-tauri/Cargo.toml");
    assert_eq!(
        bundle, console,
        "发布包名取 src-tauri/tauri.conf.json（={bundle}），控制台显示取 \
         src-tauri/Cargo.toml 的 CARGO_PKG_VERSION（={console}），两者必须同源。\
         v3.5.2 的事故就是这对值不一致：dmg/Info.plist 是 3.5.2，而编进包里的 \
         GUI crate 还是 3.5.1，于是控制台头部显示 v3.5.1。"
    );
}

/// `env!("CARGO_PKG_VERSION")` in the desktop sources belongs to `proxy-rs-gui`.
///
/// This is the fact that makes the equality above load-bearing: the console header
/// does not read `tauri.conf.json` and does not read the root crate — it reads the
/// **GUI crate's** version at compile time. If this ever fails because those call
/// sites moved to a different version source, the equality tests above stop being
/// sufficient and this file has to follow the new source.
#[test]
fn the_desktop_version_label_still_comes_from_the_gui_crate() {
    let sources: [(&str, &str); 3] = [
        (
            "src-tauri/src/app/tray.rs",
            "the console header #app-version the user sees (v3.5.2 shipped v3.5.1 here)",
        ),
        ("src-tauri/src/main.rs", "the bootstrap payload"),
        (
            "src-tauri/src/app/server.rs",
            "the in-process REST /status reply",
        ),
    ];

    for (file, meaning) in sources {
        let src = read(file);
        assert!(
            src.contains("env!(\"CARGO_PKG_VERSION\")"),
            "{file} must still report its version via env!(\"CARGO_PKG_VERSION\") \
             ({meaning}). That macro resolves to the `proxy-rs-gui` crate's version, \
             i.e. src-tauri/Cargo.toml — which is exactly why the equality assertions \
             in this file matter: the packaged version comes from \
             src-tauri/tauri.conf.json, but this label does not."
        );
    }
}

/// Cheap tripwire: a declaration site that was moved or renamed would make the
/// readers above panic with a confusing message, or worse, silently read a stale
/// copy.
#[test]
fn every_version_declaration_file_exists() {
    for file in [
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "package-lock.json",
        "src-tauri/Cargo.toml",
        "src-tauri/tauri.conf.json",
    ] {
        assert!(
            repo().join(file).is_file(),
            "{file} is a release-version declaration site and must exist"
        );
    }
}
