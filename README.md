# tar and zstd for Motor OS

`tar` and `zstd` commands that run on Motor OS and Linux and produce the same
bytes on both. They package and unpack the Motor OS toolchain, and give Motor
OS native archive tools. They are plain Rust, on
[structured-zstd](https://crates.io/crates/structured-zstd) for Zstandard and
`moto-rt` for Motor OS file permissions.

## Compatibility

`tar -c` writes archives byte-identical to those of GNU tar 1.35 run with

```
--format=gnu --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --mode=a=rX,u+w --hard-dereference
```

that is: GNU long-name records, entries sorted by name within each directory,
zero times and owners, mode 0755 for directories and executables and 0644 for
other files, record padding. `tar -x` and `tar -t` read ustar archives
(including the prefix field), GNU archives, and pax archives (`path` and
`size` records), compressed with zstd or not; `tar -t` and `tar -tv` print
what GNU tar prints, with `-tv` times in UTC.

`zstd -d` decodes what the reference zstd writes to the same bytes, and
verifies checksums. `zstd` and `tar --zstd` write deterministic frames that
any zstd decodes; for most inputs their bytes differ from the reference
implementation's, since structured-zstd is a different encoder.

Deliberate differences from GNU tar and zstd:

- only directories and regular files: links, devices, FIFOs, and sparse
  members are refused, when reading as when writing;
- names must be UTF-8; absolute names and names with `..` are refused, not
  rewritten;
- extraction does not restore times or owners, and gives Unix modes their
  Motor OS equivalent: executables become read/execute, files without
  owner write become read-only;
- extraction does not select members by name; `-C` must name an existing
  directory;
- `zstd` takes levels 1 to 19 and does not copy file modes or times to its
  outputs.

## Building

```sh
cargo build --release
cargo +TOOLCHAIN build --release --target x86_64-unknown-motor
```

where `TOOLCHAIN` is the Motor toolchain the Motor OS checkout selects.

## Testing

```sh
cargo test            # or: lorry test
```

The tests run the commands on a deterministic tree and compare their output
with reference archives that GNU tar and zstd made from the same tree on
Linux (see `tests/data/README.md`). The same tests check Motor OS against
those references; build them into one executable, copy it to a Motor OS VM,
and run it there:

```sh
lorry +TOOLCHAIN test --bundle --no-run --target x86_64-unknown-motor
```
