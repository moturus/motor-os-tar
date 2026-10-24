//! `zstd` against frames that the reference zstd wrote, and against its own
//! output made on Linux.

mod common;

use std::fs;

use common::*;

fn text() -> Vec<u8> {
    content(TEXT.0, TEXT.1)
}

fn random() -> Vec<u8> {
    content(RANDOM.0, RANDOM.1)
}

#[test]
fn decompress_reference_frames() {
    let frames: [(&str, &[u8], Vec<u8>); 9] = [
        ("level 1", include_bytes!("data/text.l1.zst"), text()),
        ("level 19", include_bytes!("data/text.l19.zst"), text()),
        (
            "level 3 with size",
            include_bytes!("data/text.l3-size.zst"),
            text(),
        ),
        (
            "level 22 with size",
            include_bytes!("data/text.l22-size.zst"),
            text(),
        ),
        (
            "no checksum",
            include_bytes!("data/text.no-check.zst"),
            text(),
        ),
        (
            "incompressible",
            include_bytes!("data/random.zst"),
            random(),
        ),
        ("empty", include_bytes!("data/empty.zst"), Vec::new()),
        (
            "two frames",
            include_bytes!("data/text-random.zst"),
            [text(), random()].concat(),
        ),
        (
            "skippable",
            include_bytes!("data/skippable-random.zst"),
            random(),
        ),
    ];
    for (name, frames, expected) in frames {
        assert_same(&run(ZSTD, &["-dc"], frames), &expected, name);
        assert_same(&run(ZSTD, &["-d"], frames), &expected, name);
        assert!(run(ZSTD, &["-t"], frames).is_empty(), "{name}");
    }
}

#[test]
fn compress_like_the_reference_zstd() {
    // For these inputs, structured-zstd writes what the reference zstd does.
    for (args, input, expected) in [
        (&["-1"][..], text(), &include_bytes!("data/text.l1.zst")[..]),
        (&["-19", "-c"], text(), include_bytes!("data/text.l19.zst")),
        (&["-q"], random(), include_bytes!("data/random.zst")),
    ] {
        assert_same(
            &run(ZSTD, args, &input),
            expected,
            &format!("zstd {args:?}"),
        );
    }
}

#[test]
fn compress_reproducibly() {
    // Elsewhere it does not; its output is checked against what it wrote on
    // Linux, which the reference zstd decodes (tests/data/README.md).
    let ours = include_bytes!("data/text.ours-l3.zst");
    assert_same(&run(ZSTD, &[], &text()), ours, "zstd");
    assert_same(
        &run(ZSTD, &["-3", "-c", "-"], &text()),
        ours,
        "zstd -3 -c -",
    );
    assert_same(&run(ZSTD, &["-dc"], ours), &text(), "decompressed");
    let empty = include_bytes!("data/empty.ours.zst");
    assert_same(&run(ZSTD, &[], b""), empty, "zstd < /dev/null");
    assert!(run(ZSTD, &["-d"], empty).is_empty());

    // A file's size is recorded in its frame.
    let dir = scratch("zstd-sizes");
    let file = dir.join("text.txt");
    fs::write(&file, text()).unwrap();
    let sized = include_bytes!("data/text.ours-l3-size.zst");
    assert_same(&run(ZSTD, &["-c", path(&file)], b""), sized, "zstd -c FILE");
    run(ZSTD, &[path(&file)], b"");
    assert_same(
        &fs::read(dir.join("text.txt.zst")).unwrap(),
        sized,
        "zstd FILE",
    );
}

#[test]
fn file_names() {
    let dir = scratch("zstd-names");
    let file = dir.join("a.tar");
    let compressed = dir.join("a.tar.zst");
    fs::write(&file, b"contents").unwrap();
    run(ZSTD, &["-q", path(&file)], b"");
    assert!(file.exists(), "the input is kept");
    let error = fails(ZSTD, &[path(&file)], b"", 1);
    assert!(error.contains("already exists"), "{error}");
    run(ZSTD, &["-f", path(&file)], b"");

    fs::remove_file(&file).unwrap();
    run(ZSTD, &["-d", path(&compressed)], b"");
    assert_eq!(fs::read(&file).unwrap(), b"contents");
    fails(ZSTD, &["-d", path(&compressed)], b"", 1);
    let tzst = dir.join("b.tzst");
    fs::rename(&compressed, &tzst).unwrap();
    run(ZSTD, &["-d", path(&tzst)], b"");
    assert_eq!(fs::read(dir.join("b.tar")).unwrap(), b"contents");
    let output = dir.join("c");
    run(ZSTD, &["-d", path(&tzst), "-o", path(&output)], b"");
    assert_eq!(fs::read(&output).unwrap(), b"contents");
    fails(ZSTD, &["-d", path(&file)], b"", 1);
    // Even forced, an input is not overwritten by its own output.
    let same = format!(
        "{}/../{}/b.tzst",
        path(&dir),
        dir.file_name().unwrap().to_str().unwrap()
    );
    fails(ZSTD, &["-df", path(&tzst), "-o", &same], b"", 1);
    run(ZSTD, &["-t", path(&tzst)], b"");

    // --rm removes inputs whose output went to a file.
    run(ZSTD, &["--rm", "-c", path(&file)], b"");
    assert!(file.exists());
    run(ZSTD, &["--rm", path(&file)], b"");
    assert!(!file.exists() && compressed.exists());
    run(ZSTD, &["-d", "--rm", path(&compressed)], b"");
    assert!(file.exists() && !compressed.exists());

    // Each input is processed, and any failure fails the command.
    let error = fails(
        ZSTD,
        &["-f", path(&dir.join("missing")), path(&file)],
        b"",
        1,
    );
    assert!(error.contains("missing"), "{error}");
    assert!(compressed.exists());
}

#[test]
fn damaged_input_is_refused() {
    let good = include_bytes!("data/text.l1.zst");
    let mut checksum = good.to_vec();
    *checksum.last_mut().unwrap() ^= 1;
    let mut block = good.to_vec();
    block[good.len() / 2] ^= 0x55;
    for (name, bad) in [
        ("checksum", checksum),
        ("block", block),
        ("truncated", good[..good.len() - 1].to_vec()),
        ("trailing junk", [&good[..], b"junk"].concat()),
        ("not zstd", b"plain text".to_vec()),
        ("empty", Vec::new()),
    ] {
        fails(ZSTD, &["-dc"], &bad, 1);
        fails(ZSTD, &["-t"], &bad, 1);
        // A partial output file is removed.
        let dir = scratch(&format!("zstd-damaged-{name}"));
        let file = dir.join("x.zst");
        fs::write(&file, &bad).unwrap();
        fails(ZSTD, &["-d", path(&file)], b"", 1);
        assert!(!dir.join("x").exists(), "{name}");
    }
}
