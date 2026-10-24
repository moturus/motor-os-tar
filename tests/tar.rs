//! `tar` against archives and listings that GNU tar made from the test tree,
//! and against its own compressed archives made on Linux.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;

const GNU: &[u8] = include_bytes!("data/top.gnu.tar");
const GNU_LIST: &str = include_str!("data/top.gnu.list");
const GNU_LISTV: &str = include_str!("data/top.gnu.listv");
const NAMES: &[u8] = include_bytes!("data/names.gnu.tar");
const USTAR: &[u8] = include_bytes!("data/top.ustar.tar");
const PAX: &[u8] = include_bytes!("data/top.pax.tar");
const DEFAULT: &[u8] = include_bytes!("data/top.default.tar");
const DEFAULT_LISTV: &str = include_str!("data/top.default.listv");
const GNU_ZSTD: &[u8] = include_bytes!("data/top.gnu.tar.zst");
const OURS_ZSTD: &[u8] = include_bytes!("data/top.tar.zst");

/// A new directory holding the test tree.
fn tree_dir(name: &str) -> PathBuf {
    let dir = scratch(name);
    materialize(&dir);
    dir
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap()
}

#[test]
fn create_matches_gnu_tar() {
    let dir = tree_dir("create");
    let archive = dir.join("top.tar");
    run(TAR, &["-cf", path(&archive), "-C", path(&dir), "top"], b"");
    assert_same(&fs::read(&archive).unwrap(), GNU, "tar -cf");
    assert_same(
        &run(TAR, &["-c", "-C", path(&dir), "top"], b""),
        GNU,
        "tar -c",
    );
    assert_same(
        &run(TAR, &["cf", "-", "-C", path(&dir), "top/"], b""),
        GNU,
        "tar cf -",
    );
    let names = dir.join("top/names");
    assert_same(
        &run(TAR, &["-cC", path(&names), "."], b""),
        NAMES,
        "tar -c .",
    );

    // Verbose names, as -t lists them, go to standard output, or to standard
    // error when the archive does.
    let output = spawn(TAR, &["-cvf", path(&archive), "-C", path(&dir), "top"], b"");
    assert!(output.status.success());
    assert_eq!(text(output.stdout), GNU_LIST);
    let output = spawn(TAR, &["-cvC", path(&dir), "top"], b"");
    assert_same(&output.stdout, GNU, "tar -cv");
    assert_eq!(text(output.stderr), GNU_LIST);
}

#[test]
fn create_compressed() {
    let dir = tree_dir("create-zstd");
    for (args, name) in [
        (&["--zstd", "-cf"][..], "a.tar.zst"),
        (&["-caf"], "b.tar.zst"),
        (&["-a", "-cf"], "c.tzst"),
    ] {
        let archive = dir.join(name);
        let args = [args, &[path(&archive), "-C", path(&dir), "top"]].concat();
        run(TAR, &args, b"");
        assert_same(&fs::read(&archive).unwrap(), OURS_ZSTD, name);
    }
    assert_same(
        &run(TAR, &["--zstd", "-cC", path(&dir), "top"], b""),
        OURS_ZSTD,
        "stdout",
    );
    // Uncompressed, it is GNU tar's archive.
    assert_same(&run(ZSTD, &["-dc"], OURS_ZSTD), GNU, "decompressed");
    // Only zstd compression is supported.
    let archive = dir.join("d.tar.gz");
    fails(
        TAR,
        &["-caf", path(&archive), "-C", path(&dir), "top"],
        b"",
        2,
    );
    // Without -a, a name does not select compression, as with GNU tar.
    let archive = dir.join("e.tar.zst");
    run(TAR, &["-cf", path(&archive), "-C", path(&dir), "top"], b"");
    assert_same(&fs::read(&archive).unwrap(), GNU, "tar -cf e.tar.zst");
}

#[test]
fn extract_archives_of_every_format() {
    for (name, archive, readonly) in [
        ("gnu", GNU, false),
        ("ustar", USTAR, false),
        ("pax", PAX, false),
        ("default", DEFAULT, true),
        ("gnu-zstd", GNU_ZSTD, false),
        ("ours-zstd", OURS_ZSTD, false),
    ] {
        let dir = scratch(&format!("extract-{name}"));
        let file = dir.join("archive");
        fs::write(&file, archive).unwrap();
        let out = dir.join("file");
        fs::create_dir(&out).unwrap();
        run(TAR, &["-xf", path(&file), "-C", path(&out)], b"");
        verify(&out, readonly);
        let out = dir.join("stdin");
        fs::create_dir(&out).unwrap();
        run(TAR, &["-x", "-C", path(&out)], archive);
        verify(&out, readonly);
    }
}

#[test]
fn extracted_trees_archive_identically() {
    let dir = scratch("round-trip");
    let output = spawn(TAR, &["-xvC", path(&dir)], GNU);
    assert!(output.status.success());
    assert_eq!(text(output.stdout), GNU_LIST);
    assert_same(
        &run(TAR, &["-cC", path(&dir), "top"], b""),
        GNU,
        "re-created",
    );
    // Extracting again replaces the files, read-only ones too.
    run(TAR, &["-xC", path(&dir)], DEFAULT);
    verify(&dir, true);
    run(TAR, &["-xC", path(&dir)], DEFAULT);
    verify(&dir, true);
    assert_same(
        &run(TAR, &["-cC", path(&dir), "top"], b""),
        GNU,
        "re-created again",
    );
}

#[test]
fn list_matches_gnu_tar() {
    for archive in [GNU, USTAR, PAX, GNU_ZSTD, OURS_ZSTD] {
        assert_eq!(text(run(TAR, &["-t"], archive)), GNU_LIST);
    }
    assert_eq!(text(run(TAR, &["-tv"], GNU)), GNU_LISTV);
    assert_eq!(text(run(TAR, &["tvf", "-"], DEFAULT)), DEFAULT_LISTV);
}

#[test]
fn unsafe_members_are_refused() {
    for (name, archive) in [
        ("absolute", &include_bytes!("data/absolute.tar")[..]),
        ("dotdot", include_bytes!("data/dotdot.tar")),
        ("symlink", include_bytes!("data/symlink.tar")),
    ] {
        let dir = scratch(&format!("unsafe-{name}"));
        let out = dir.join("out");
        fs::create_dir(&out).unwrap();
        let error = fails(TAR, &["-x", "-C", path(&out)], archive, 2);
        assert!(
            error.contains("refusing") || error.contains("unsupported"),
            "{error}"
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "{name}");
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0, "{name}");
    }
}

#[test]
fn damaged_archives_are_refused() {
    let dir = scratch("damaged");
    let mut header = GNU.to_vec();
    header[0] ^= 1;
    assert!(fails(TAR, &["-t"], &header, 2).contains("checksum"));
    let truncated = &GNU[..GNU.len() / 2];
    assert!(fails(TAR, &["-xC", path(&dir)], truncated, 2).contains("unexpected end"));
    // The end of a compressed archive is checked too: its checksum and that
    // nothing follows it.
    let mut checksum = OURS_ZSTD.to_vec();
    *checksum.last_mut().unwrap() ^= 1;
    fails(TAR, &["-t"], &checksum, 2);
    fails(TAR, &["-t"], &[OURS_ZSTD, b"junk"].concat(), 2);
    fails(TAR, &["-t"], &GNU_ZSTD[..GNU_ZSTD.len() - 1], 2);
}

#[test]
fn bad_create_operands_are_refused() {
    let dir = tree_dir("create-refused");
    for operand in ["/", "/top", "..", "top/../top", "", "missing"] {
        fails(TAR, &["-cf", "-", "-C", path(&dir), operand], b"", 2);
    }
    fails(TAR, &["-c", "-f", "-"], b"", 2);
    fails(TAR, &["-x", "-C", path(&dir.join("missing"))], GNU, 2);
}

#[cfg(unix)]
#[test]
fn unsupported_files_are_refused() {
    let dir = scratch("create-symlink");
    fs::create_dir(dir.join("top")).unwrap();
    std::os::unix::fs::symlink("target", dir.join("top/link")).unwrap();
    let error = fails(TAR, &["-cf", "-", "-C", path(&dir), "top"], b"", 2);
    assert!(error.contains("top/link: unsupported file type"), "{error}");
}

/// Writes the test tree into the empty directory named by
/// MOTOR_OS_TAR_TREE, to make the reference archives from.
#[test]
#[ignore]
fn write_tree() {
    let root = std::env::var("MOTOR_OS_TAR_TREE").expect("MOTOR_OS_TAR_TREE is not set");
    materialize(Path::new(&root));
}
