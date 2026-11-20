//! Architecture guard: dependencies only point inward, layers stay I/O-free.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

fn assert_absent(dir: &str, forbidden: &[&str]) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(dir);
    for file in rust_files(&root) {
        let body = fs::read_to_string(&file).expect("read source");
        for needle in forbidden {
            assert!(
                !body.contains(needle),
                "{} must not reference {needle}",
                file.display()
            );
        }
    }
}

#[test]
fn domain_has_no_outer_layers_or_io() {
    assert_absent(
        "domain",
        &[
            "crate::adapters",
            "crate::application",
            "crate::entry",
            "std::fs",
            "std::process",
            "std::net",
            "std::io",
            "println!",
        ],
    );
}

#[test]
fn application_depends_on_domain_only() {
    assert_absent(
        "application",
        &[
            "crate::adapters",
            "crate::entry",
            "std::fs",
            "std::process",
            "std::net",
            "std::io",
            "println!",
        ],
    );
}

#[test]
fn adapters_do_not_reach_outward() {
    assert_absent("adapters", &["crate::entry", "crate::application"]);
}

#[test]
fn source_forbids_unsafe_code() {
    // Cargo.toml already forbids `unsafe`; this catches blocks added under an
    // explicit allow in a module.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in rust_files(&root) {
        let body = fs::read_to_string(&file).expect("read source");
        assert!(
            !body.contains("unsafe "),
            "{} must not use unsafe code",
            file.display()
        );
    }
}
