//! tar: creates, extracts, and lists archives of directories and regular
//! files, with a subset of GNU tar's command line.
//!
//! Created archives are deterministic, so they are identical on every
//! platform and byte-identical to GNU tar's with the options given in
//! `motor_os_tar::archive`.

use std::borrow::Cow;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use motor_os_tar::archive::{self, Kind, Member, RECORD_SIZE, Reader, Writer};
use motor_os_tar::{platform, zst};

const USAGE: &str = "\
Usage: tar -c [-v] [-a | --zstd] [-f ARCHIVE] [-C DIR] FILE...
       tar -x [-v] [-f ARCHIVE] [-C DIR]
       tar -t [-v] [-f ARCHIVE]

  -c          create an archive of FILEs, directories recursively
  -x          extract an archive into the current directory or DIR
  -t          list the members of an archive
  -f ARCHIVE  read or write ARCHIVE; '-' or none means standard input/output
  -C DIR      change to DIR first; with -c, applies to the FILEs that follow
  -v          print member names; with -t, in long format (times in UTC)
  -a          with -c, compress with zstd if ARCHIVE ends in .zst or .tzst
  --zstd      with -c, compress with zstd

Archives compressed with zstd are recognized when read. Created archives use
GNU's format, with sorted names, zero owners and times, and modes 0755 for
directories and executables and 0644 for other files. Only directories and
regular files are supported.
";

/// Suffixes of compression formats other than zstd, refused by -a.
const OTHER_COMPRESSION: [&str; 13] = [
    ".gz", ".tgz", ".taz", ".Z", ".bz2", ".tbz", ".tbz2", ".xz", ".txz", ".lz", ".lzma", ".tlz",
    ".lz4",
];

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Create,
    Extract,
    List,
}

struct Options {
    mode: Mode,
    archive: Option<String>,
    verbose: bool,
    auto_compress: bool,
    zstd: bool,
    directory: PathBuf,
    /// With -c: each FILE and the directory it is relative to.
    files: Vec<(PathBuf, String)>,
}

fn main() -> ExitCode {
    let options = match parse(env::args_os().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("tar: {message}\nTry 'tar --help' for more information.");
            return ExitCode::from(2);
        }
    };
    let result = match options.mode {
        Mode::Create => create(&options),
        Mode::Extract => extract(&options),
        Mode::List => list(&options),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tar: {error}");
            ExitCode::from(2)
        }
    }
}

/// Parses the command line; `None` asks for help.
fn parse(args: impl Iterator<Item = std::ffi::OsString>) -> Result<Option<Options>, String> {
    let mut args = args
        .map(|arg| {
            arg.into_string()
                .map_err(|arg| format!("{}: argument is not valid UTF-8", arg.to_string_lossy()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Traditional usage, "tar cvf ARCHIVE ...", takes option arguments from
    // the words that follow.
    if args.first().is_some_and(|arg| !arg.starts_with('-')) {
        let letters = args.remove(0);
        let mut rest = args.into_iter();
        args = Vec::new();
        for letter in letters.chars() {
            args.push(format!("-{letter}"));
            if matches!(letter, 'f' | 'C') {
                args.extend(rest.next());
            }
        }
        args.extend(rest);
    }

    let mut mode = None;
    let mut options = Options {
        mode: Mode::Create,
        archive: None,
        verbose: false,
        auto_compress: false,
        zstd: false,
        directory: PathBuf::from("."),
        files: Vec::new(),
    };
    let mut args = args.into_iter();
    let mut only_files = false;
    while let Some(arg) = args.next() {
        if only_files || arg == "-" || !arg.starts_with('-') {
            options.files.push((options.directory.clone(), arg));
            continue;
        }
        match arg.as_str() {
            "--" => only_files = true,
            "--help" => return Ok(None),
            "--zstd" => options.zstd = true,
            "--auto-compress" => options.auto_compress = true,
            _ if arg.starts_with("--") => return Err(format!("unrecognized option '{arg}'")),
            _ => {
                let mut letters = arg[1..].chars();
                while let Some(letter) = letters.next() {
                    match letter {
                        'c' | 'x' | 't' => {
                            let new = match letter {
                                'c' => Mode::Create,
                                'x' => Mode::Extract,
                                _ => Mode::List,
                            };
                            if mode.is_some_and(|mode| mode != new) {
                                return Err(
                                    "you may not specify more than one of -c, -x, -t".into()
                                );
                            }
                            mode = Some(new);
                        }
                        'v' => options.verbose = true,
                        'a' => options.auto_compress = true,
                        'f' | 'C' => {
                            let mut value: String = letters.by_ref().collect();
                            if value.is_empty() {
                                value = args.next().ok_or_else(|| {
                                    format!("option requires an argument -- '{letter}'")
                                })?;
                            }
                            if letter == 'f' {
                                options.archive = Some(value);
                            } else {
                                options.directory = options.directory.join(value);
                            }
                        }
                        _ => return Err(format!("invalid option -- '{letter}'")),
                    }
                }
            }
        }
    }

    options.mode = mode.ok_or("you must specify one of -c, -x, -t")?;
    match options.mode {
        Mode::Create if options.files.is_empty() => {
            Err("cowardly refusing to create an empty archive".into())
        }
        Mode::Extract | Mode::List if !options.files.is_empty() => {
            Err("selecting members by name is not supported".into())
        }
        _ => Ok(Some(options)),
    }
}

fn create(options: &Options) -> io::Result<()> {
    let to_stdout = matches!(options.archive.as_deref(), None | Some("-"));
    let zstd = options.zstd || (options.auto_compress && zstd_suffix(options.archive.as_deref())?);
    let out: Box<dyn Write> = match options.archive.as_deref() {
        Some(path) if !to_stdout => Box::new(File::create(path).map_err(at(path))?),
        _ => Box::new(io::stdout().lock()),
    };
    let out = BufWriter::with_capacity(RECORD_SIZE as usize, out);
    // Names go to standard error when the archive goes to standard output.
    let mut log: Box<dyn Write> = match (options.verbose, to_stdout) {
        (false, _) => Box::new(io::sink()),
        (true, true) => Box::new(io::stderr()),
        (true, false) => Box::new(io::stdout()),
    };
    let mut out = if zstd {
        let encoder = zst::encoder(out, zst::DEFAULT_LEVEL, None)?;
        write_archive(encoder, &options.files, &mut log)?.finish()?
    } else {
        write_archive(out, &options.files, &mut log)?
    };
    out.flush()?;
    log.flush()
}

/// With -a: whether ARCHIVE's name asks for zstd compression.
fn zstd_suffix(archive: Option<&str>) -> io::Result<bool> {
    let name = archive.unwrap_or("-");
    if name.ends_with(".zst") || name.ends_with(".tzst") {
        return Ok(true);
    }
    if OTHER_COMPRESSION
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return Err(io::Error::other(format!(
            "{name}: only zstd compression is supported"
        )));
    }
    Ok(false)
}

fn write_archive<W: Write>(
    out: W,
    files: &[(PathBuf, String)],
    log: &mut dyn Write,
) -> io::Result<W> {
    let mut writer = Writer::new(out);
    for (directory, file) in files {
        // GNU tar names members after FILE without its trailing slashes.
        let name = file.trim_end_matches('/');
        if file.starts_with('/') {
            return Err(io::Error::other(format!(
                "{file}: refusing to archive an absolute path"
            )));
        }
        if name.is_empty() || name.split('/').any(|component| component == "..") {
            return Err(io::Error::other(format!(
                "'{file}': refusing to archive an empty path or one containing '..'"
            )));
        }
        add(&mut writer, &directory.join(name), name, log)?;
    }
    writer.finish()
}

/// Appends `path` as a member named `name`, directories recursively with
/// their entries sorted by name, as GNU tar's `--sort=name` does.
fn add<W: Write>(
    writer: &mut Writer<W>,
    path: &Path,
    name: &str,
    log: &mut dyn Write,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(at(name))?;
    if metadata.is_dir() {
        writeln!(log, "{}/", quote(name))?;
        writer.append_dir(name)?;
        let mut entries = fs::read_dir(path)
            .map_err(at(name))?
            .map(|entry| {
                entry?.file_name().into_string().map_err(|entry| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{name}/{}: name is not valid UTF-8",
                            entry.to_string_lossy()
                        ),
                    )
                })
            })
            .collect::<io::Result<Vec<_>>>()
            .map_err(at(name))?;
        entries.sort_unstable();
        for entry in entries {
            add(writer, &path.join(&entry), &format!("{name}/{entry}"), log)?;
        }
    } else if metadata.is_file() {
        let file = File::open(path).map_err(at(name))?;
        let size = file.metadata().map_err(at(name))?.len();
        let executable = platform::is_executable(&file).map_err(at(name))?;
        writeln!(log, "{}", quote(name))?;
        writer
            .append_file(name, executable, size, file)
            .map_err(at(name))?;
    } else {
        return Err(io::Error::other(format!(
            "{name}: unsupported file type; only directories and regular files are supported"
        )));
    }
    Ok(())
}

fn extract(options: &Options) -> io::Result<()> {
    let destination = &options.directory;
    if !destination.is_dir() {
        let destination = destination.display();
        return Err(io::Error::other(format!("{destination}: not a directory")));
    }
    let mut reader = Reader::new(open_archive(options.archive.as_deref())?);
    let mut log: Box<dyn Write> = if options.verbose {
        Box::new(io::stdout().lock())
    } else {
        Box::new(io::sink())
    };
    while let Some(member) = reader.next_member()? {
        writeln!(log, "{}", quote(&member.name))?;
        let relative = archive::member_path(&member.name)?;
        let path = destination.join(&relative);
        let result = match member.kind {
            Kind::Directory if relative.as_os_str().is_empty() => Ok(()),
            Kind::Directory => make_dir(&path, member.mode),
            Kind::File if relative.as_os_str().is_empty() => {
                Err(io::Error::other("not a valid file name"))
            }
            Kind::File => extract_file(&mut reader, &path, member.mode),
        };
        result.map_err(at(&member.name))?;
    }
    finish_input(reader)?;
    log.flush()
}

fn make_dir(path: &Path, mode: u32) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    make_parent(path)?;
    platform::create_dir(path, mode)
}

/// Creates missing parent directories. Existing ones are not created again:
/// Motor OS refuses that when their own parent is not writable.
fn make_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.is_dir() => fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

fn extract_file(reader: &mut Reader<impl Read>, path: &Path, mode: u32) -> io::Result<()> {
    make_parent(path)?;
    // Like GNU tar, replace an existing file rather than write through it.
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(io::Error::other("a directory is in the way"));
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = platform::create_file(path, mode)?;
    io::copy(reader, &mut file)?;
    platform::finish_file(&file, mode)
}

fn list(options: &Options) -> io::Result<()> {
    let mut reader = Reader::new(open_archive(options.archive.as_deref())?);
    let mut out = BufWriter::new(io::stdout().lock());
    // The initial width of the "user/group size" column, as observed with
    // GNU tar 1.35; it grows to fit wider entries.
    let mut width = 18;
    while let Some(member) = reader.next_member()? {
        if options.verbose {
            writeln!(out, "{}", long_listing(&member, &mut width))?;
        } else {
            writeln!(out, "{}", quote(&member.name))?;
        }
    }
    finish_input(reader)?;
    out.flush()
}

/// Opens an archive, decompressing it if it starts with a zstd frame.
fn open_archive(path: Option<&str>) -> io::Result<Box<dyn Read>> {
    let mut input: Box<dyn Read> = match path {
        None | Some("-") => Box::new(io::stdin().lock()),
        Some(path) => {
            let file = File::open(path).map_err(at(path))?;
            Box::new(BufReader::with_capacity(RECORD_SIZE as usize, file))
        }
    };
    let mut magic = Vec::new();
    (&mut input).take(4).read_to_end(&mut magic)?;
    let compressed = zst::is_zstd(&magic);
    let input = io::Cursor::new(magic).chain(input);
    if compressed {
        Ok(Box::new(zst::decoder(input)))
    } else {
        Ok(Box::new(input))
    }
}

/// Reads the input to its end, so that compressed archives are verified
/// whole: their checksums, and that nothing follows.
fn finish_input(reader: Reader<impl Read>) -> io::Result<()> {
    io::copy(&mut reader.into_inner(), &mut io::sink()).map(drop)
}

/// A line of `tar -tv` output as GNU tar lays it out, with the time in UTC.
fn long_listing(member: &Member, width: &mut usize) -> String {
    let mode = member.mode;
    let bit = |mask: u32, set: char| if mode & mask != 0 { set } else { '-' };
    let execute = |mask: u32, special: u32, set: char| match (mode & mask, mode & special) {
        (0, 0) => '-',
        (_, 0) => 'x',
        (0, _) => set.to_ascii_uppercase(),
        _ => set,
    };
    let modes: String = [
        if member.kind == Kind::Directory {
            'd'
        } else {
            '-'
        },
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        execute(0o100, 0o4000, 's'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        execute(0o010, 0o2000, 's'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        execute(0o001, 0o1000, 't'),
    ]
    .iter()
    .collect();
    let owner = |name: &str, id: u64| {
        if name.is_empty() {
            id.to_string()
        } else {
            name.to_owned()
        }
    };
    let user = owner(&member.uname, member.uid);
    let group = owner(&member.gname, member.gid);
    let size = member.size.to_string();
    let used = user.len() + 1 + group.len() + size.len();
    *width = (*width).max(used);
    format!(
        "{modes} {user}/{group} {size:>pad$} {} {}",
        utc(member.mtime),
        quote(&member.name),
        pad = *width - used + size.len(),
    )
}

/// Formats seconds since the epoch as "YYYY-MM-DD HH:MM" in UTC.
fn utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let minutes = seconds % 86_400 / 60;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let (era, day_of_era) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        minutes / 60,
        minutes % 60
    )
}

/// Escapes a name as GNU tar's default quoting style does in a UTF-8 locale.
fn quote(name: &str) -> Cow<'_, str> {
    if !name.chars().any(|c| c == '\\' || c.is_control()) {
        return Cow::Borrowed(name);
    }
    let mut quoted = String::new();
    for c in name.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '\x07' => quoted.push_str("\\a"),
            '\x08' => quoted.push_str("\\b"),
            '\x0c' => quoted.push_str("\\f"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\x0b' => quoted.push_str("\\v"),
            c if c.is_control() => {
                for byte in c.encode_utf8(&mut [0; 4]).bytes() {
                    let _ = write!(quoted, "\\{byte:03o}");
                }
            }
            c => quoted.push(c),
        }
    }
    Cow::Owned(quoted)
}

/// Prefixes an error with the name it concerns.
fn at(name: &str) -> impl FnOnce(io::Error) -> io::Error + '_ {
    move |error| io::Error::new(error.kind(), format!("{name}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Option<Options>, String> {
        parse(args.iter().map(|arg| arg.into()))
    }

    #[test]
    fn command_lines() {
        let options = parsed(&["-cvf", "a.tar", "-C", "dir", "x", "-C", "sub", "y"])
            .unwrap()
            .unwrap();
        assert!(options.mode == Mode::Create && options.verbose);
        assert_eq!(options.archive.as_deref(), Some("a.tar"));
        let files: Vec<_> = options
            .files
            .iter()
            .map(|(d, f)| (d.to_str().unwrap(), f.as_str()))
            .collect();
        assert_eq!(files, [("./dir", "x"), ("./dir/sub", "y")]);

        let options = parsed(&["xaf", "a.tar.zst", "-Cout"]).unwrap().unwrap();
        assert!(options.mode == Mode::Extract && options.auto_compress);
        assert_eq!(options.archive.as_deref(), Some("a.tar.zst"));
        assert_eq!(options.directory, Path::new("./out"));

        let error = parsed(&["-tf-", "--", "-x"]).err().unwrap();
        assert!(error.contains("members by name"), "{error}");
        assert!(parsed(&["--help"]).unwrap().is_none());
        for bad in [
            &["-cx", "a"][..],
            &["-c"],
            &["-cf"],
            &["-cz", "a"],
            &["--gzip"],
            &["-t"][..0],
        ] {
            assert!(parsed(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn long_listing_layout() {
        let mut member = Member {
            name: "dir/".into(),
            kind: Kind::Directory,
            mode: 0o755,
            size: 0,
            uid: 0,
            gid: 0,
            uname: String::new(),
            gname: String::new(),
            mtime: 0,
        };
        let mut width = 18;
        assert_eq!(
            long_listing(&member, &mut width),
            "drwxr-xr-x 0/0               0 1970-01-01 00:00 dir/"
        );
        member.kind = Kind::File;
        member.mode = 0o7644;
        member.uname = "a-long-user-name".into();
        member.gname = "group".into();
        member.size = 1234;
        member.mtime = 1_729_728_000 + 13 * 3600 + 7 * 60 + 59;
        assert_eq!(
            long_listing(&member, &mut width),
            "-rwSr-Sr-T a-long-user-name/group 1234 2024-10-24 13:07 dir/"
        );
        assert_eq!(width, 26);
    }

    #[test]
    fn dates() {
        assert_eq!(utc(0), "1970-01-01 00:00");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00");
        assert_eq!(utc(4_107_542_399), "2100-02-28 23:59");
        assert_eq!(utc(0o777_7777_7777), "2242-03-16 12:56");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("plain name é 日本"), "plain name é 日本");
        assert_eq!(quote("a\\b\nc\td\x01e\x7f"), "a\\\\b\\nc\\td\\001e\\177");
        assert_eq!(quote("\u{85}"), "\\302\\205");
    }
}
