//! The tar format.
//!
//! [`Writer`] produces GNU-format archives byte-identical to those of GNU tar
//! 1.35 run with `--format=gnu --sort=name --mtime=@0 --owner=0 --group=0
//! --numeric-owner --mode=a=rX,u+w --hard-dereference`, given members in the
//! order GNU tar visits them. [`Reader`] reads ustar, GNU, and pax archives of
//! directories and regular files.

use std::io::{self, Read, Write};
use std::path::PathBuf;

pub const BLOCK_SIZE: usize = 512;
/// GNU tar's default record size (blocking factor 20); archives are padded
/// to a whole number of records.
pub const RECORD_SIZE: u64 = 20 * BLOCK_SIZE as u64;

/// The longest name the header holds; longer ones get a GNU long-name record.
const NAME_LEN: usize = 100;
/// The largest member size the header's 11 octal digits hold.
const MAX_SIZE: u64 = 0o777_7777_7777;
/// Bound on the GNU long-name records and pax headers a reader accepts.
const META_LIMIT: u64 = 1 << 20;
/// Bound on member sizes a reader accepts, far above any real member, which
/// keeps size arithmetic from overflowing.
const SIZE_LIMIT: u64 = 1 << 62;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
}

/// A member's header, with GNU long-name and pax records applied.
#[derive(Debug)]
pub struct Member {
    /// The name as stored; directories usually end with a slash.
    pub name: String,
    pub kind: Kind,
    pub mode: u32,
    pub size: u64,
    pub uid: u64,
    pub gid: u64,
    pub uname: String,
    pub gname: String,
    pub mtime: u64,
}

pub struct Writer<W: Write> {
    out: W,
    len: u64,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W) -> Self {
        Self { out, len: 0 }
    }

    /// Appends a directory; `name` has no trailing slash.
    pub fn append_dir(&mut self, name: &str) -> io::Result<()> {
        self.append_header(&format!("{name}/"), 0o755, 0, b'5')
    }

    /// Appends a regular file of `size` bytes, read from `data`.
    pub fn append_file(
        &mut self,
        name: &str,
        executable: bool,
        size: u64,
        mut data: impl Read,
    ) -> io::Result<()> {
        let mode = if executable { 0o755 } else { 0o644 };
        self.append_header(name, mode, size, b'0')?;
        let copied = io::copy(&mut (&mut data).take(size), &mut self.out)?;
        self.len += copied;
        let mut extra = Vec::new();
        data.take(1).read_to_end(&mut extra)?;
        if copied != size || !extra.is_empty() {
            return Err(io::Error::other("file changed as we read it"));
        }
        self.pad()
    }

    /// Writes the end-of-archive marker and the record padding, and returns
    /// the output without flushing it.
    pub fn finish(mut self) -> io::Result<W> {
        self.write(&[0; 2 * BLOCK_SIZE])?;
        let padding = self.len.next_multiple_of(RECORD_SIZE) - self.len;
        self.write(&vec![0; padding as usize])?;
        Ok(self.out)
    }

    fn append_header(&mut self, name: &str, mode: u32, size: u64, kind: u8) -> io::Result<()> {
        if name.len() > NAME_LEN {
            let long_name = [name.as_bytes(), b"\0"].concat();
            self.write(&header(
                b"././@LongLink",
                0o644,
                long_name.len() as u64,
                b'L',
            )?)?;
            self.write(&long_name)?;
            self.pad()?;
        }
        self.write(&header(name.as_bytes(), mode, size, kind)?)
    }

    fn pad(&mut self) -> io::Result<()> {
        let padding = self.len.next_multiple_of(BLOCK_SIZE as u64) - self.len;
        self.write(&[0; BLOCK_SIZE][..padding as usize])
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.len += bytes.len() as u64;
        Ok(())
    }
}

/// A GNU-format header with zero uid, gid, and mtime, and no owner names.
fn header(name: &[u8], mode: u32, size: u64, kind: u8) -> io::Result<[u8; BLOCK_SIZE]> {
    if size > MAX_SIZE {
        return Err(io::Error::other("member is too large (8 GiB or more)"));
    }
    let mut block = [0; BLOCK_SIZE];
    let len = name.len().min(NAME_LEN);
    block[..len].copy_from_slice(&name[..len]);
    put_octal(&mut block[100..108], mode.into());
    put_octal(&mut block[108..116], 0);
    put_octal(&mut block[116..124], 0);
    put_octal(&mut block[124..136], size);
    put_octal(&mut block[136..148], 0);
    block[156] = kind;
    block[257..265].copy_from_slice(b"ustar  \0");
    set_checksum(&mut block);
    Ok(block)
}

/// Fills the checksum field as GNU tar does: six digits, a NUL, a space.
fn set_checksum(block: &mut [u8; BLOCK_SIZE]) {
    block[148..156].fill(b' ');
    let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
    put_octal(&mut block[148..155], sum.into());
}

/// Writes zero-padded octal digits and a terminating NUL.
fn put_octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    field[..digits].copy_from_slice(format!("{value:0digits$o}").as_bytes());
    field[digits] = 0;
}

/// Reads members in turn; as [`Read`], it yields the current member's data.
pub struct Reader<R: Read> {
    input: R,
    /// Unread data of the current member.
    data: u64,
    /// The padding after it.
    padding: u64,
}

impl<R: Read> Reader<R> {
    pub fn new(input: R) -> Self {
        Self {
            input,
            data: 0,
            padding: 0,
        }
    }

    pub fn into_inner(self) -> R {
        self.input
    }

    /// Returns the next member, skipping what is left of the current one, or
    /// `None` at the end of the archive.
    pub fn next_member(&mut self) -> io::Result<Option<Member>> {
        self.skip(self.data + self.padding)?;
        (self.data, self.padding) = (0, 0);
        let mut long_name = None;
        let mut pax = Pax::default();
        loop {
            let Some(block) = self.read_block()? else {
                // GNU tar also accepts an archive that ends without the marker.
                if long_name.is_some() || pax != Pax::default() {
                    return Err(unexpected_eof());
                }
                return Ok(None);
            };
            if block.iter().all(|&b| b == 0) {
                return Ok(None);
            }
            check_checksum(&block)?;
            let size = number(&block[124..136])?;
            match block[156] {
                b'L' => {
                    let mut name = self.read_meta(size)?;
                    while name.last() == Some(&0) {
                        name.pop();
                    }
                    long_name = Some(utf8(name)?);
                }
                b'x' => pax.update(&self.read_meta(size)?)?,
                b'g' | b'K' => drop(self.read_meta(size)?),
                kind @ (b'0' | b'\0' | b'7' | b'5') => {
                    let name = match pax.path.take().or(long_name.take()) {
                        Some(name) => name,
                        None => header_name(&block)?,
                    };
                    let size = pax.size.unwrap_or(size);
                    if size > SIZE_LIMIT {
                        return Err(invalid(format!("{name}: member size is too large")));
                    }
                    // Pre-POSIX archives mark directories with a slash only.
                    let kind = if kind == b'5' || name.ends_with('/') {
                        Kind::Directory
                    } else {
                        Kind::File
                    };
                    (self.data, self.padding) = (size, padding(size));
                    return Ok(Some(Member {
                        name,
                        kind,
                        mode: number(&block[100..108])? as u32 & 0o7777,
                        size,
                        uid: number(&block[108..116])?,
                        gid: number(&block[116..124])?,
                        uname: String::from_utf8_lossy(text(&block[265..297])).into_owned(),
                        gname: String::from_utf8_lossy(text(&block[297..329])).into_owned(),
                        mtime: number(&block[136..148])?,
                    }));
                }
                kind => {
                    let name = match pax.path.or(long_name) {
                        Some(name) => name,
                        None => header_name(&block)?,
                    };
                    let kind = kind.escape_ascii();
                    return Err(invalid(format!(
                        "{name}: unsupported member type '{kind}'; only directories and regular files are supported"
                    )));
                }
            }
        }
    }

    /// Reads a whole block, or returns `None` at the end of the input.
    fn read_block(&mut self) -> io::Result<Option<[u8; BLOCK_SIZE]>> {
        let mut block = [0; BLOCK_SIZE];
        let mut filled = 0;
        while filled < BLOCK_SIZE {
            match self.input.read(&mut block[filled..]) {
                Ok(0) if filled == 0 => return Ok(None),
                Ok(0) => return Err(unexpected_eof()),
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Some(block))
    }

    /// Reads the data of a long-name record or pax header.
    fn read_meta(&mut self, size: u64) -> io::Result<Vec<u8>> {
        if size > META_LIMIT {
            return Err(invalid("extended header is too large"));
        }
        let mut data = Vec::new();
        (&mut self.input).take(size).read_to_end(&mut data)?;
        if data.len() as u64 != size {
            return Err(unexpected_eof());
        }
        self.skip(padding(size))?;
        Ok(data)
    }

    fn skip(&mut self, len: u64) -> io::Result<()> {
        if io::copy(&mut (&mut self.input).take(len), &mut io::sink())? != len {
            return Err(unexpected_eof());
        }
        Ok(())
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.data == 0 {
            return Ok(0);
        }
        let len = buf
            .len()
            .min(usize::try_from(self.data).unwrap_or(usize::MAX));
        let n = self.input.read(&mut buf[..len])?;
        if n == 0 {
            return Err(unexpected_eof());
        }
        self.data -= n as u64;
        Ok(n)
    }
}

/// The pax extended header records this reader applies.
#[derive(Default, PartialEq)]
struct Pax {
    path: Option<String>,
    size: Option<u64>,
}

impl Pax {
    /// Applies records of the form "LENGTH KEY=VALUE\n".
    fn update(&mut self, mut data: &[u8]) -> io::Result<()> {
        let malformed = || invalid("malformed pax extended header");
        while !data.is_empty() {
            let space = data.iter().position(|&b| b == b' ').ok_or_else(malformed)?;
            let len = decimal(&data[..space]).ok_or_else(malformed)?;
            let len = usize::try_from(len).map_err(|_| malformed())?;
            if len <= space + 1 || len > data.len() || data[len - 1] != b'\n' {
                return Err(malformed());
            }
            let record = &data[space + 1..len - 1];
            let eq = record
                .iter()
                .position(|&b| b == b'=')
                .ok_or_else(malformed)?;
            let (key, value) = (&record[..eq], &record[eq + 1..]);
            // An empty value cancels the record, per POSIX.
            match key {
                b"path" if value.is_empty() => self.path = None,
                b"path" => self.path = Some(utf8(value.to_vec())?),
                b"size" if value.is_empty() => self.size = None,
                b"size" => self.size = Some(decimal(value).ok_or_else(malformed)?),
                _ if key.starts_with(b"GNU.sparse.") => {
                    return Err(invalid("sparse members are not supported"));
                }
                _ => {}
            }
            data = &data[len..];
        }
        Ok(())
    }
}

/// The name field, joined with the prefix field in POSIX ustar headers.
fn header_name(block: &[u8; BLOCK_SIZE]) -> io::Result<String> {
    let name = text(&block[..100]);
    let prefix = text(&block[345..500]);
    if &block[257..263] == b"ustar\0" && !prefix.is_empty() {
        return utf8([prefix, b"/", name].concat());
    }
    utf8(name.to_vec())
}

fn check_checksum(block: &[u8; BLOCK_SIZE]) -> io::Result<()> {
    let stored = number(&block[148..156])?;
    let (mut unsigned, mut signed) = (0u64, 0i64);
    for (i, &b) in block.iter().enumerate() {
        let b = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += u64::from(b);
        signed += i64::from(b as i8);
    }
    // Some historic tars summed signed bytes; GNU tar accepts both.
    if stored != unsigned && i64::try_from(stored) != Ok(signed) {
        return Err(invalid("header checksum mismatch"));
    }
    Ok(())
}

/// Parses a numeric field: octal digits, possibly space-padded, or GNU's
/// base-256 form for values too large for octal.
fn number(field: &[u8]) -> io::Result<u64> {
    let malformed = || invalid("malformed number in header");
    if field[0] & 0x80 != 0 {
        // Bit 6 of the first byte is the sign; negative values are refused.
        if field[0] & 0x40 != 0 {
            return Err(malformed());
        }
        let mut value = u64::from(field[0] & 0x3f);
        for &b in &field[1..] {
            if value >> 56 != 0 {
                return Err(malformed());
            }
            value = value << 8 | u64::from(b);
        }
        return Ok(value);
    }
    let mut bytes = field.iter().copied().skip_while(|&b| b == b' ').peekable();
    let mut value = 0u64;
    while let Some(digit) = bytes.next_if(|b| (b'0'..=b'7').contains(b)) {
        value = value.checked_mul(8).ok_or_else(malformed)? + u64::from(digit - b'0');
    }
    if bytes.any(|b| b != b' ' && b != 0) {
        return Err(malformed());
    }
    Ok(value)
}

fn decimal(text: &[u8]) -> Option<u64> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// A NUL-terminated field's contents.
fn text(field: &[u8]) -> &[u8] {
    field.split(|&b| b == 0).next().unwrap_or_default()
}

fn utf8(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes).map_err(|e| {
        let name = String::from_utf8_lossy(e.as_bytes()).into_owned();
        invalid(format!("{name}: member name is not valid UTF-8"))
    })
}

fn padding(size: u64) -> u64 {
    size.next_multiple_of(BLOCK_SIZE as u64) - size
}

/// The path, relative to the extraction directory, that a member named
/// `name` extracts to. Names that could leave that directory are refused.
pub fn member_path(name: &str) -> io::Result<PathBuf> {
    if name.starts_with('/') {
        return Err(invalid(format!(
            "{name}: refusing to extract an absolute name"
        )));
    }
    let mut path = PathBuf::new();
    for component in name.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(invalid(format!(
                    "{name}: refusing to extract a name containing '..'"
                )));
            }
            component => path.push(component),
        }
    }
    Ok(path)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn unexpected_eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected end of archive")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(build: impl FnOnce(&mut Writer<Vec<u8>>)) -> Vec<u8> {
        let mut writer = Writer::new(Vec::new());
        build(&mut writer);
        writer.finish().unwrap()
    }

    fn members(archive: &[u8]) -> io::Result<Vec<(Member, Vec<u8>)>> {
        let mut reader = Reader::new(archive);
        let mut members = Vec::new();
        while let Some(member) = reader.next_member()? {
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;
            members.push((member, data));
        }
        Ok(members)
    }

    /// A raw member: a header (rewritten by `edit`) and padded data.
    fn raw(
        name: &[u8],
        kind: u8,
        data: &[u8],
        edit: impl FnOnce(&mut [u8; BLOCK_SIZE]),
    ) -> Vec<u8> {
        let mut block = header(name, 0o644, data.len() as u64, kind).unwrap();
        edit(&mut block);
        set_checksum(&mut block);
        let mut bytes = block.to_vec();
        bytes.extend_from_slice(data);
        bytes.resize(bytes.len().next_multiple_of(BLOCK_SIZE), 0);
        bytes
    }

    #[test]
    fn round_trip() {
        let long = format!("top/{}/file", "d".repeat(120));
        let bytes = archive(|w| {
            w.append_dir("top").unwrap();
            w.append_file("top/a", true, 3, &b"abc"[..]).unwrap();
            w.append_file(&long, false, 0, &b""[..]).unwrap();
        });
        assert_eq!(bytes.len() as u64 % RECORD_SIZE, 0);
        let members = members(&bytes).unwrap();
        let names: Vec<_> = members.iter().map(|(m, _)| m.name.as_str()).collect();
        assert_eq!(names, ["top/", "top/a", long.as_str()]);
        assert_eq!(members[0].0.kind, Kind::Directory);
        assert_eq!((members[1].0.mode, &members[1].1[..]), (0o755, &b"abc"[..]));
        assert_eq!(members[2].0.mode, 0o644);
    }

    #[test]
    fn file_size_must_match() {
        let mut w = Writer::new(Vec::new());
        assert!(w.append_file("short", false, 4, &b"abc"[..]).is_err());
        let mut w = Writer::new(Vec::new());
        assert!(w.append_file("long", false, 2, &b"abc"[..]).is_err());
    }

    #[test]
    fn numbers() {
        assert_eq!(number(b"0000644\0").unwrap(), 0o644);
        assert_eq!(number(b"   644 \0").unwrap(), 0o644);
        assert_eq!(number(b"\0\0\0\0\0\0\0\0").unwrap(), 0);
        assert_eq!(number(b"77777777777\0").unwrap(), MAX_SIZE);
        let mut big = [0u8; 12];
        big[0] = 0x80;
        big[7..].copy_from_slice(&[0x02, 0, 0, 0, 1]);
        assert_eq!(number(&big).unwrap(), (2 << 32) + 1);
        assert!(number(b"0000x44\0").is_err());
        assert!(number(&[0xff; 12]).is_err());
    }

    #[test]
    fn pax_records_override_the_header() {
        let path = format!("{}/name", "p".repeat(150));
        let record = |key: &str, value: &str| {
            let body = format!(" {key}={value}\n");
            let mut len = body.len() + 1;
            while len.to_string().len() + body.len() != len {
                len += 1;
            }
            format!("{len}{body}")
        };
        let pax = record("path", &path) + &record("size", "5") + &record("mtime", "1.5");
        let mut bytes = raw(b"PaxHeaders/x", b'x', pax.as_bytes(), |_| {});
        // The ustar header's own size field claims nothing.
        bytes.extend(raw(b"short", b'0', b"", |_| {}));
        bytes.extend_from_slice(b"hello");
        bytes.resize(bytes.len() + 512 - 5 + 1024, 0);
        let members = members(&bytes).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].0.name, path);
        assert_eq!(members[0].1, b"hello");
    }

    #[test]
    fn malformed_pax_is_refused() {
        for pax in [
            &b"5 a=b\n"[..],
            b"99 path=x\n",
            b"8 path\n\n",
            b"x path=a\n",
        ] {
            let bytes = raw(b"PaxHeaders/x", b'x', pax, |_| {});
            assert!(members(&bytes).is_err(), "{}", pax.escape_ascii());
        }
        let bytes = raw(b"x", b'x', b"22 GNU.sparse.major=1\n", |_| {});
        assert!(members(&bytes).unwrap_err().to_string().contains("sparse"));
        let mut bytes = raw(b"x", b'x', b"29 size=18446744073709551615\n", |_| {});
        bytes.extend(raw(b"huge", b'0', b"", |_| {}));
        assert!(
            members(&bytes)
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
    }

    #[test]
    fn ustar_prefix_is_joined() {
        let bytes = raw(b"name", b'0', b"", |block| {
            block[257..265].copy_from_slice(b"ustar\x0000");
            block[345..351].copy_from_slice(b"prefix");
        });
        assert_eq!(members(&bytes).unwrap()[0].0.name, "prefix/name");
    }

    #[test]
    fn unsupported_and_corrupt_members_are_refused() {
        for kind in *b"12346SD" {
            let err = members(&raw(b"link", kind, b"", |_| {})).unwrap_err();
            assert!(err.to_string().contains("unsupported member type"), "{err}");
        }
        let mut bytes = archive(|w| w.append_file("a", false, 1, &b"x"[..]).unwrap());
        bytes[0] = b'b';
        assert!(
            members(&bytes)
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
        let bytes = archive(|w| w.append_file("a", false, 600, &[7u8; 600][..]).unwrap());
        let err = members(&bytes[..1000]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(members(&raw(b"\xff", b'0', b"", |_| {})).is_err());
    }

    #[test]
    fn end_marker_is_optional() {
        let bytes = archive(|w| w.append_file("a", false, 1, &b"x"[..]).unwrap());
        assert_eq!(members(&bytes[..1024]).unwrap().len(), 1);
        assert!(members(&raw(b"././@LongLink", b'L', b"name\0", |_| {})).is_err());
    }

    #[test]
    fn old_style_directories() {
        let members = members(&raw(b"dir/", b'0', b"", |_| {})).unwrap();
        assert_eq!(members[0].0.kind, Kind::Directory);
    }

    #[test]
    fn member_paths() {
        assert_eq!(member_path("./a//b/").unwrap(), PathBuf::from("a/b"));
        assert_eq!(member_path("./").unwrap(), PathBuf::new());
        assert!(member_path("/etc/passwd").is_err());
        assert!(member_path("a/../../b").is_err());
        assert!(member_path("..").is_err());
        assert_eq!(member_path("a/..b").unwrap(), PathBuf::from("a/..b"));
    }
}
