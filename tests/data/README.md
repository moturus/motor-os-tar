# Reference data

The integration tests compare what `tar` and `zstd` produce, on Linux and
on Motor OS, with these files. They were made on Linux, with GNU tar 1.35
and zstd 1.5.7, from the test tree that `tests/common/mod.rs` defines, by
the commands below. Run from the repository root, they regenerate every
file and check that the reference zstd decodes what these tools write.
Every file but `top.default.tar` and its listing, which record when the
tree was made, comes out the same each time.

| Files | Made by | Contents |
|---|---|---|
| `top.gnu.tar`, `names.gnu.tar` | GNU tar | the tree and `top/names` (as `.`), in the deterministic GNU format that `tar -c` must reproduce byte for byte |
| `top.gnu.list`, `top.gnu.listv`, `top.default.listv` | GNU tar | `tar -t` and `tar -tv` listings |
| `top.ustar.tar`, `top.pax.tar` | GNU tar | the tree in the ustar format (long names split into prefix and name) and the pax format (long and non-ASCII names in pax records, a global header) |
| `top.default.tar` | GNU tar | the tree with GNU tar's defaults: actual modes and times, directory order, owner names |
| `top.gnu.tar.zst` | GNU tar and zstd | `top.gnu.tar`, compressed by `tar --zstd` |
| `absolute.tar`, `dotdot.tar`, `symlink.tar` | GNU tar | members that `tar -x` must refuse |
| `text.*.zst`, `random.zst`, `empty.zst`, `text-random.zst`, `skippable-random.zst` | zstd | `top/data/text.txt` and `top/data/random.bin` at several levels and settings, nothing, two frames, a skippable frame and a frame |
| `top.tar.zst`, `*.ours*.zst` | this `tar` and `zstd` | their compressed output where it differs from the reference zstd's (level 3, frames recording the content size, empty input); it must be the same on every platform |

For this text at levels 1 and 19, and for incompressible input, `zstd`
writes exactly what the reference zstd writes, and the tests compare it
with `text.l1.zst`, `text.l19.zst`, and `random.zst`. That is a property of
these inputs and the pinned structured-zstd, not of every input.

```sh
set -e
T=$(mktemp -d)
MOTOR_OS_TAR_TREE=$T cargo test --test tar -- --ignored write_tree
cargo build --release
D=tests/data
N='--sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --mode=a=rX,u+w --hard-dereference'
PAX='exthdr.name=%d/PaxHeaders/%f,globexthdr.name=pax_global_header,globexthdr.mtime=0,delete=atime,delete=ctime,comment=motor-os-tar'
export LC_ALL=C.UTF-8 TZ=UTC

tar --format=gnu $N -cf $D/top.gnu.tar -C $T top
tar -tf $D/top.gnu.tar > $D/top.gnu.list
tar -tvf $D/top.gnu.tar > $D/top.gnu.listv
tar --format=gnu $N -cf $D/names.gnu.tar -C $T/top/names .
tar --format=ustar $N -cf $D/top.ustar.tar -C $T top
tar --format=posix --pax-option=$PAX $N -cf $D/top.pax.tar -C $T top
tar --format=gnu --owner=nobody:65534 --group=nogroup:65534 -cf $D/top.default.tar -C $T top
tar -tvf $D/top.default.tar > $D/top.default.listv
tar --format=gnu $N --zstd -cf $D/top.gnu.tar.zst -C $T top
zstd -dc $D/top.gnu.tar.zst | cmp - $D/top.gnu.tar

(cd $T && touch evil && ln -s evil link)
tar $N -P --transform='s,^,/,' -cf $D/absolute.tar -C $T evil
tar $N -P --transform='s,^,../,' -cf $D/dotdot.tar -C $T evil
tar $N -cf $D/symlink.tar -C $T link

TEXT=$T/top/data/text.txt BINARY=$T/top/data/random.bin
zstd -q -1 -c < $TEXT > $D/text.l1.zst
zstd -q -19 -c < $TEXT > $D/text.l19.zst
zstd -q -3 -c $TEXT > $D/text.l3-size.zst
zstd -q --ultra -22 -c $TEXT > $D/text.l22-size.zst
zstd -q --no-check -c < $TEXT > $D/text.no-check.zst
zstd -q -c < $BINARY > $D/random.zst
zstd -q -c < /dev/null > $D/empty.zst
cat $D/text.l1.zst $D/random.zst > $D/text-random.zst
printf 'P*M\030\005\000\000\000hello' | cat - $D/random.zst > $D/skippable-random.zst

target/release/tar --zstd -cf $D/top.tar.zst -C $T top
target/release/zstd -q -c < $TEXT > $D/text.ours-l3.zst
target/release/zstd -q -c $TEXT > $D/text.ours-l3-size.zst
target/release/zstd -q -c < /dev/null > $D/empty.ours.zst
zstd -dc $D/top.tar.zst | cmp - $D/top.gnu.tar
for f in $D/text.ours-*.zst; do zstd -dc $f | cmp - $TEXT; done
zstd -dc $D/empty.ours.zst | cmp - /dev/null
target/release/zstd -q -1 -c < $TEXT | cmp - $D/text.l1.zst
target/release/zstd -q -19 -c < $TEXT | cmp - $D/text.l19.zst
target/release/zstd -q -c < $BINARY | cmp - $D/random.zst
rm -r $T
```
