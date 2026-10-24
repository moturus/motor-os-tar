//! zstd: compresses and decompresses files in the Zstandard format, with a
//! subset of the zstd command line.
//!
//! Decompressed output is byte-identical to the reference zstd's. Compressed
//! output is deterministic and decodable by any zstd, but its bytes differ
//! from the reference implementation's.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, IsTerminal, Read, Write};
use std::process::ExitCode;

use motor_os_tar::zst;

const USAGE: &str = "\
Usage: zstd [-d | -t] [-c | -o FILE] [-f] [-k | --rm] [-q] [-#] [FILE...]

Compresses each FILE into FILE.zst, or with -d decompresses FILE.zst into
FILE and FILE.tzst into FILE.tar. Without FILEs, or for '-', reads standard
input and writes standard output.

  -d       decompress
  -t       test compressed FILEs: decompress them and discard the output
  -c       write to standard output
  -o FILE  write to FILE; one input only
  -f       overwrite existing output files
  -k       keep input files (the default)
  --rm     remove input files after writing their output files
  -q       quiet (accepted for compatibility)
  -#       compression level, 1 to 19 (default 3)
";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Compress,
    Decompress,
    Test,
}

struct Options {
    mode: Mode,
    stdout: bool,
    output: Option<String>,
    force: bool,
    remove: bool,
    level: i32,
    files: Vec<String>,
}

fn main() -> ExitCode {
    let options = match parse(env::args_os().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("zstd: {message}\nTry 'zstd --help' for more information.");
            return ExitCode::FAILURE;
        }
    };
    let stdin = ["-".to_owned()];
    let files = if options.files.is_empty() {
        &stdin[..]
    } else {
        &options.files
    };
    let mut status = ExitCode::SUCCESS;
    for file in files {
        if let Err(error) = process(&options, file) {
            eprintln!("zstd: {file}: {error}");
            status = ExitCode::FAILURE;
        }
    }
    status
}

/// Parses the command line; `None` asks for help.
fn parse(args: impl Iterator<Item = OsString>) -> Result<Option<Options>, String> {
    let mut options = Options {
        mode: Mode::Compress,
        stdout: false,
        output: None,
        force: false,
        remove: false,
        level: zst::DEFAULT_LEVEL,
        files: Vec::new(),
    };
    let mut args = args.map(|arg| {
        arg.into_string()
            .map_err(|arg| format!("{}: argument is not valid UTF-8", arg.to_string_lossy()))
    });
    let mut only_files = false;
    while let Some(arg) = args.next() {
        let arg = arg?;
        if only_files || arg == "-" || !arg.starts_with('-') {
            options.files.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => only_files = true,
            "--help" => return Ok(None),
            "--rm" => options.remove = true,
            _ if arg.starts_with("--") => return Err(format!("unrecognized option '{arg}'")),
            _ => {
                let mut letters = arg[1..].chars().peekable();
                while let Some(letter) = letters.next() {
                    match letter {
                        'z' => options.mode = Mode::Compress,
                        'd' => options.mode = Mode::Decompress,
                        't' => options.mode = Mode::Test,
                        'c' => options.stdout = true,
                        'f' => options.force = true,
                        'k' => options.remove = false,
                        'q' => {}
                        'o' => {
                            let mut value: String = letters.by_ref().collect();
                            if value.is_empty() {
                                value =
                                    args.next().ok_or("option requires an argument -- 'o'")??;
                            }
                            options.output = Some(value);
                        }
                        '0'..='9' => {
                            let mut digits = String::from(letter);
                            while let Some(digit) = letters.next_if(char::is_ascii_digit) {
                                digits.push(digit);
                            }
                            options.level = digits
                                .parse()
                                .ok()
                                .filter(|level| (1..=19).contains(level))
                                .ok_or(format!(
                                    "invalid compression level -{digits}; use 1 to 19"
                                ))?;
                        }
                        _ => return Err(format!("invalid option -- '{letter}'")),
                    }
                }
            }
        }
    }
    if options.output.is_some() && (options.stdout || options.files.len() > 1) {
        return Err("-o takes a single input and excludes -c".into());
    }
    Ok(Some(options))
}

fn process(options: &Options, file: &str) -> io::Result<()> {
    let output = match &options.output {
        _ if options.mode == Mode::Test || options.stdout => None,
        Some(output) => Some(output.clone()),
        None if file == "-" => None,
        None => Some(output_name(options.mode, file)?),
    };
    if output.is_none() && options.mode == Mode::Compress && io::stdout().is_terminal() {
        return Err(io::Error::other("stdout is a console, aborting"));
    }
    let (input, size): (Box<dyn Read>, _) = if file == "-" {
        (Box::new(io::stdin().lock()), None)
    } else {
        let input = File::open(file)?;
        let metadata = input.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        (Box::new(BufReader::new(input)), Some(metadata.len()))
    };

    let Some(output) = output else {
        return match options.mode {
            Mode::Test => convert(options, input, size, io::sink()),
            _ => convert(options, input, size, io::stdout().lock()),
        };
    };
    if let (Ok(input), Ok(output)) = (fs::canonicalize(file), fs::canonicalize(&output))
        && input == output
    {
        return Err(io::Error::other("the input and output are the same file"));
    }
    let mut open = OpenOptions::new();
    open.write(true);
    if options.force {
        open.create(true).truncate(true);
    } else {
        open.create_new(true);
    }
    let out = open.open(&output).map_err(|error| match error.kind() {
        io::ErrorKind::AlreadyExists => io::Error::new(
            error.kind(),
            format!("{output} already exists; not overwritten without -f"),
        ),
        _ => io::Error::new(error.kind(), format!("{output}: {error}")),
    })?;
    // A partial output must not pass for a complete one.
    if let Err(error) = convert(options, input, size, BufWriter::new(out)) {
        let _ = fs::remove_file(&output);
        return Err(error);
    }
    if options.remove && file != "-" {
        fs::remove_file(file)?;
    }
    Ok(())
}

/// The output file for `file`, named as the zstd command names it.
fn output_name(mode: Mode, file: &str) -> io::Result<String> {
    if mode == Mode::Compress {
        return Ok(format!("{file}.zst"));
    }
    if let Some(stem) = file.strip_suffix(".zst").filter(|stem| !stem.is_empty()) {
        return Ok(stem.to_owned());
    }
    if let Some(stem) = file.strip_suffix(".tzst") {
        return Ok(format!("{stem}.tar"));
    }
    Err(io::Error::other(
        "unknown suffix (.zst or .tzst expected); specify the output with -o or -c",
    ))
}

fn convert(
    options: &Options,
    mut input: impl Read,
    size: Option<u64>,
    mut out: impl Write,
) -> io::Result<()> {
    if options.mode == Mode::Compress {
        let mut encoder = zst::encoder(out, options.level, size)?;
        io::copy(&mut input, &mut encoder)?;
        return encoder.finish()?.flush();
    }
    // An empty input holds no frame, which the zstd command reports.
    let mut first = Vec::new();
    (&mut input).take(1).read_to_end(&mut first)?;
    if first.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of file",
        ));
    }
    io::copy(
        &mut zst::decoder(io::Cursor::new(first).chain(input)),
        &mut out,
    )?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Option<Options>, String> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn command_lines() {
        let options = parsed(&["-dqf19", "-oout", "in"]).unwrap().unwrap();
        assert!(options.mode == Mode::Decompress && options.force);
        assert_eq!(options.level, 19);
        assert_eq!(options.output.as_deref(), Some("out"));
        assert_eq!(options.files, ["in"]);

        let options = parsed(&["--rm", "-c", "--", "-x", "-"]).unwrap().unwrap();
        assert!(options.remove && options.stdout);
        assert_eq!(options.files, ["-x", "-"]);
        assert!(parsed(&["--help"]).unwrap().is_none());
        for bad in [
            &["-0"][..],
            &["-20"],
            &["-o"],
            &["-y"],
            &["--long"],
            &["-o", "x", "a", "b"],
        ] {
            assert!(parsed(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn output_names() {
        assert_eq!(output_name(Mode::Compress, "a.tar").unwrap(), "a.tar.zst");
        assert_eq!(output_name(Mode::Decompress, "dir/a.zst").unwrap(), "dir/a");
        assert_eq!(output_name(Mode::Decompress, "a.tzst").unwrap(), "a.tar");
        assert!(output_name(Mode::Decompress, "a.gz").is_err());
        assert!(output_name(Mode::Decompress, ".zst").is_err());
    }
}
