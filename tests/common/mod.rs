//! The deterministic test tree and helpers shared by the integration tests.
//!
//! The reference archives in tests/data were made from this tree on Linux,
//! as tests/data/README.md describes, so the tree must not change without
//! regenerating them.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub const TAR: &str = env!("CARGO_BIN_EXE_tar");
pub const ZSTD: &str = env!("CARGO_BIN_EXE_zstd");

pub const TEXT: (&str, usize) = ("top/data/text.txt", 200_000);
pub const RANDOM: (&str, usize) = ("top/data/random.bin", 40_000);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Node {
    Dir,
    File {
        len: usize,
        exec: bool,
        readonly: bool,
    },
}

const fn file(len: usize) -> Node {
    Node::File {
        len,
        exec: false,
        readonly: false,
    }
}

const fn exec(len: usize) -> Node {
    Node::File {
        len,
        exec: true,
        readonly: false,
    }
}

/// The tree under `top`, parents before children. It holds every kind of
/// name and size the archive format treats specially, and only names that
/// both Linux and Motor OS accept.
pub fn tree() -> Vec<(String, Node)> {
    let mut tree: Vec<(String, Node)> = [
        ("top", Node::Dir),
        ("top/README.txt", file(1500)),
        ("top/bin", Node::Dir),
        ("top/bin/run.sh", exec(300)),
        ("top/bin/tool", exec(9000)),
        ("top/data", Node::Dir),
        (RANDOM.0, file(RANDOM.1)),
        (TEXT.0, file(TEXT.1)),
        ("top/empty", Node::Dir),
        ("top/empty-file", file(0)),
        (
            "top/readonly.txt",
            Node::File {
                len: 100,
                exec: false,
                readonly: true,
            },
        ),
        // Sorting compares names within each directory, byte by byte.
        ("top/names", Node::Dir),
        ("top/names/A", file(1)),
        ("top/names/Z", file(2)),
        ("top/names/_x", file(3)),
        ("top/names/a", file(4)),
        ("top/names/a b", file(5)),
        ("top/names/a-b", file(6)),
        ("top/names/a.b", file(7)),
        ("top/names/ab", file(8)),
        ("top/names/d", Node::Dir),
        ("top/names/d/inner", file(9)),
        ("top/names/d-e", file(10)),
        ("top/names/d.txt", file(11)),
        ("top/names/héllo", file(12)),
        ("top/names/x~", file(13)),
        ("top/names/日本語.txt", file(14)),
        ("top/names/🦀", file(15)),
        // Sizes around block and record boundaries.
        ("top/sizes", Node::Dir),
        ("top/sizes/1", file(1)),
        ("top/sizes/511", file(511)),
        ("top/sizes/512", file(512)),
        ("top/sizes/513", file(513)),
        ("top/sizes/1024", file(1024)),
        ("top/sizes/10239", file(10239)),
        ("top/sizes/10240", file(10240)),
        ("top/sizes/10241", file(10241)),
        ("top/long", Node::Dir),
    ]
    .into_iter()
    .map(|(path, node)| (path.to_owned(), node))
    .collect();
    // Names of 100 and 101 bytes (with a directory's slash), around the
    // header's limit, and a 255-byte one that ustar can still split.
    let long = |c: &str, n: usize| format!("top/long/{}", c.repeat(n));
    let (deep_dir, deeper_dir) = (
        long("e", 99),
        format!("{}/{}", long("e", 99), "f".repeat(46)),
    );
    tree.extend([
        (long("a", 91), file(100)),
        (long("b", 92), file(101)),
        (long("c", 90), Node::Dir),
        (long("d", 91), Node::Dir),
        (format!("{}/x", long("d", 91)), file(102)),
        (deep_dir, Node::Dir),
        (format!("{deeper_dir}/{}", "g".repeat(99)), file(255)),
        (deeper_dir, Node::Dir),
    ]);
    tree.sort_by(|a, b| a.0.cmp(&b.0));
    tree
}

/// The contents of the file at `path`: text, or random bytes for binaries.
pub fn content(path: &str, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(path);
    if path.ends_with(".bin") || path.ends_with("/tool") {
        return (0..len).map(|_| rng.next() as u8).collect();
    }
    const WORDS: [&str; 16] = [
        "motor",
        "tar",
        "zstd",
        "archive",
        "block",
        "header",
        "record",
        "frame",
        "the",
        "of",
        "a",
        "and",
        "kernel",
        "rust",
        "file",
        "directory",
    ];
    let mut text = Vec::with_capacity(len + 200);
    let mut line = 0;
    while text.len() < len {
        line += 1;
        write!(text, "{path} {line}:").unwrap();
        for _ in 0..rng.next() % 12 {
            write!(text, " {}", WORDS[(rng.next() % 16) as usize]).unwrap();
        }
        text.push(b'\n');
    }
    text.truncate(len);
    text
}

/// SplitMix64, seeded with the FNV-1a hash of a string.
struct Rng(u64);

impl Rng {
    fn new(seed: &str) -> Self {
        let hash = seed.bytes().fold(0xcbf2_9ce4_8422_2325, |hash: u64, b| {
            (hash ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
        Rng(hash)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let z = (self.0 ^ (self.0 >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        let z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// Creates the tree in `root`.
pub fn materialize(root: &Path) {
    for (path, node) in tree() {
        let full = root.join(&path);
        match node {
            Node::Dir => fs::create_dir(&full).unwrap(),
            Node::File {
                len,
                exec,
                readonly,
            } => {
                fs::write(&full, content(&path, len)).unwrap();
                if exec {
                    set_executable(&full);
                }
                if readonly {
                    let mut permissions = fs::metadata(&full).unwrap().permissions();
                    permissions.set_readonly(true);
                    fs::set_permissions(&full, permissions).unwrap();
                }
            }
        }
    }
}

/// Asserts that `root` holds exactly the tree: its names, kinds, contents,
/// and executable bits, and with `readonly`, which non-executable files are
/// read-only. (Motor OS finalizes every executable as read-only.)
pub fn verify(root: &Path, readonly: bool) {
    let mut found = BTreeMap::new();
    walk(root, root, &mut found);
    let expected: BTreeMap<String, Node> = tree().into_iter().collect();
    assert_eq!(
        found.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "names in {}",
        root.display()
    );
    for (path, node) in expected {
        let full = root.join(&path);
        match node {
            Node::Dir => assert!(found[&path], "{path} is not a directory"),
            Node::File {
                len,
                exec,
                readonly: expected_readonly,
            } => {
                assert!(!found[&path], "{path} is not a file");
                assert!(
                    fs::read(&full).unwrap() == content(&path, len),
                    "{path}: contents"
                );
                assert_eq!(is_executable(&full), exec, "{path}: executable bit");
                if readonly && !exec {
                    let actual = fs::metadata(&full).unwrap().permissions().readonly();
                    assert_eq!(actual, expected_readonly, "{path}: read-only bit");
                }
            }
        }
    }
}

/// Records each entry below `dir` by its path relative to `root`, and whether
/// it is a directory.
fn walk(root: &Path, dir: &Path, found: &mut BTreeMap<String, bool>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let is_dir = entry.file_type().unwrap().is_dir();
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        found.insert(relative, is_dir);
        if is_dir {
            walk(root, &path, found);
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
}

#[cfg(target_os = "motor")]
fn set_executable(path: &Path) {
    use moto_rt::fs::{PERM_EXEC, PERM_READ};
    moto_rt::fs::set_perm(path.to_str().unwrap(), PERM_READ | PERM_EXEC).unwrap();
}

#[cfg(target_os = "motor")]
pub fn is_executable(path: &Path) -> bool {
    let attr = moto_rt::fs::stat(path.to_str().unwrap()).unwrap();
    attr.perm & moto_rt::fs::PERM_EXEC != 0
}

/// A new, empty directory for one test.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// Runs `program`, feeding it `stdin`.
pub fn spawn(program: &str, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    // A separate writer, so that a full output pipe cannot stall the input.
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&stdin);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    output
}

/// Runs `program` like `spawn`, checks that it succeeded, and returns its
/// standard output.
#[track_caller]
pub fn run(program: &str, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let output = spawn(program, args, stdin);
    assert!(
        output.status.success(),
        "{program} {args:?}: {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Runs `program` like `spawn` and checks that it failed with `code`.
#[track_caller]
pub fn fails(program: &str, args: &[&str], stdin: &[u8], code: i32) -> String {
    let output = spawn(program, args, stdin);
    assert_eq!(output.status.code(), Some(code), "{program} {args:?}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Asserts equal bytes, reporting where they first differ.
#[track_caller]
pub fn assert_same(actual: &[u8], expected: &[u8], what: &str) {
    if actual != expected {
        let offset = actual
            .iter()
            .zip(expected)
            .take_while(|(a, b)| a == b)
            .count();
        panic!(
            "{what}: {} bytes, differing at offset {offset} from the expected {} bytes",
            actual.len(),
            expected.len()
        );
    }
}
