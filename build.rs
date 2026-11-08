//! Generate the console's compiled-in asset manifest from the built UI on disk.
//!
//! ## Where the assets come from
//!
//! The front-end is built by Vite into `ui-dist/` (`npm run build:ui`, or
//! `task ui-build`), and *that* is what ships: `tauri.conf.json`'s
//! `frontendDist` points at it, and this script embeds it. `ui/` is the source
//! tree and is never read here.
//!
//! The output layout is fixed by `vite.config.js` so the three served roots
//! below keep working unchanged: chunks land in `ui-dist/src/`, stylesheets in
//! `ui-dist/style/`, everything else in `ui-dist/assets/`. That is a contract
//! between this file, `vite.config.js` and the routes in `src/console.rs`; a
//! change to one that is not reflected in the others fails
//! `ui/tools/check-dist.mjs` before it can become a 404 at runtime.
//!
//! ## Why a build script instead of a hand-maintained list
//!
//! `src/console.rs` serves the *same* front-end the Tauri window loads, from
//! bytes compiled into the binary (`include_str!`), so the deployment stays one
//! executable plus a data directory. That part is deliberate and unchanged.
//!
//! What used to be hand-maintained is the *list*: 47 `(url, content-type,
//! include_str!)` triples written out one by one. Splitting a UI module meant
//! remembering to add a line for every new file, and forgetting one produced a
//! 404 at runtime — not a compile error, not a failing test. Generating the list
//! here removes that class of bug: a file the build emits is served, and one
//! deleted stops being served.
//!
//! `ui/bootstrap.html` (the Tauri start page) and `ui/server/` are not in
//! `ui-dist/` and not console assets; `bootstrap.html` is read by Tauri
//! directly, and `server/login.html` / `login.js` are `include_str!` by
//! `console.rs` from `ui/server/`.
//!
//! The output is `${OUT_DIR}/ui_assets.rs`, included by `src/console.rs` as the
//! `ASSETS` constant. `src/console.rs` carries a test that walks the same roots
//! and fails if a file of a served kind is missing from the manifest, so an
//! extension this script does not know about is loud rather than silent.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    // Rebuild when the built UI changes, including additions and deletions.
    // Cargo scans a directory argument recursively for this purpose. This is
    // checked before the existence test below so that creating `ui-dist/` for
    // the first time also triggers a rerun.
    println!("cargo:rerun-if-changed=ui-dist");
    println!("cargo:rerun-if-changed=build.rs");

    let dist = Path::new("ui-dist");
    if !dist.is_dir() {
        panic!(
            "ui-dist/ not found. The console assets are built by Vite, not read from ui/ — \
             run `npm install && npm run build:ui` (or `task ui-build`) before `cargo build`."
        );
    }

    // (directory in ui-dist/, URL prefix). Order is irrelevant — `static_asset`
    // looks up by exact path — but output is sorted for a reproducible file.
    let roots: &[(&str, &str)] = &[
        ("src", "/src/"),
        ("style", "/style/"),
        ("assets", "/assets/"),
    ];

    let mut entries: Vec<(String, String, PathBuf)> = Vec::new();

    // The shell document is served from a route that does not map onto a
    // directory, so it is named explicitly.
    let index = dist.join("index.html");
    if index.is_file() {
        entries.push((
            "/index.html".to_string(),
            "text/html; charset=utf-8".to_string(),
            absolute(&index),
        ));
    }

    // The server bridge. It is **not** part of the bundle — `console.rs` injects
    // it as a classic `<script>` before the module graph, and Vite has no reason
    // to emit a file the page never imports. It still lives in `ui/`, so it is
    // read from there and served under the `/src/` prefix the injected tag uses.
    // The login page's files are `include_str!` directly by `console.rs`.
    let bridge = Path::new("ui/server/bridge.js");
    if bridge.is_file() {
        entries.push((
            "/src/bridge.js".to_string(),
            "text/javascript; charset=utf-8".to_string(),
            absolute(bridge),
        ));
    }

    for (root, prefix) in roots {
        let dir = dist.join(root);
        let mut files = Vec::new();
        walk(&dir, &mut files);
        for path in files {
            let Ok(rel) = path.strip_prefix(&dir) else {
                continue;
            };
            // `to_string_lossy` would paper over a non-UTF-8 file name; the URL
            // space is ASCII by construction, so such a file is not servable.
            let Some(rel) = rel.to_str() else {
                println!("cargo:warning=skipping non-UTF-8 asset path in ui-dist/{root}");
                continue;
            };
            let url = format!("{prefix}{}", rel.replace('\\', "/"));
            let Some(ct) = content_type(&path) else {
                // A file of an unknown kind is almost certainly a mistake (an
                // editor backup, a `.map` left by a build tweak). Fail loudly
                // instead of shipping a manifest that silently omits it.
                panic!(
                    "ui-dist/{root}/{rel} has an extension this build script does not know. \
                     Add it to `content_type()` in build.rs, or remove the file."
                );
            };
            entries.push((url, ct, absolute(&path)));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::new();
    out.push_str(
        "// @generated by build.rs from ui/ — do not edit by hand.\n\
         //\n\
         // Regenerated on every build; see build.rs for the served roots.\n\
         pub const ASSETS: &[(&str, &str, &str)] = &[\n",
    );
    for (url, ct, path) in &entries {
        // An absolute path is required: `include_str!` resolves relative to the
        // file containing it, which is in OUT_DIR, not the crate root.
        writeln!(
            out,
            "    ({:?}, {:?}, include_str!({:?})),",
            url,
            ct,
            path.to_string_lossy()
        )
        .unwrap();
    }
    out.push_str("];\n");

    let dest = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("ui_assets.rs");
    std::fs::write(&dest, out).expect("write ui_assets.rs");
}

/// Recursively collect regular files, sorted, so the manifest is stable.
///
/// Symlinks are skipped: a link to a file outside the crate would compile into
/// the binary without appearing in the repository, which is a supply-chain
/// surprise for anyone reading the diff.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            println!("cargo:warning=skipping symlinked asset {}", path.display());
        } else if meta.is_dir() {
            walk(&path, out);
        } else if meta.is_file() {
            // macOS drops `.DS_Store` into every directory it visits.
            if path.file_name().is_some_and(|n| n == ".DS_Store") {
                continue;
            }
            out.push(path);
        }
    }
    out.sort();
}

/// The `Content-Type` for a served asset, or `None` for a kind we do not serve.
fn content_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let ct = match ext.as_str() {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "html" => "text/html; charset=utf-8",
        _ => return None,
    };
    Some(ct.to_string())
}

/// Absolute, canonicalized path — `include_str!` in OUT_DIR needs one.
fn absolute(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()))
}
