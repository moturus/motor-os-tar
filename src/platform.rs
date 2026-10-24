//! File permissions, which Unix and Motor OS represent differently.
//!
//! Archives carry Unix modes. Motor OS has read/write/execute permissions per
//! role instead: a new file starts read/write for its creator, and a finished
//! executable becomes read/execute, since Motor refuses a direct change from
//! read/write to read/write/execute.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

/// Whether an open regular file is executable.
pub fn is_executable(file: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(file.metadata()?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(target_os = "motor")]
    {
        use std::os::fd::AsRawFd;
        let attr = moto_rt::fs::get_file_attr(file.as_raw_fd()).map_err(motor_error)?;
        Ok(attr.perm & moto_rt::fs::PERM_EXEC != 0)
    }
    #[cfg(not(any(unix, target_os = "motor")))]
    {
        let _ = file;
        Ok(false)
    }
}

/// Creates a new file for an extracted member of archive mode `mode`.
pub fn create_file(path: &Path, mode: u32) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode & 0o777);
    }
    #[cfg(not(unix))]
    let _ = mode;
    options.open(path)
}

/// Applies what creation could not of `mode` to a fully written file.
pub fn finish_file(file: &File, mode: u32) -> io::Result<()> {
    #[cfg(target_os = "motor")]
    {
        use moto_rt::fs::{PERM_EXEC, PERM_READ};
        use std::os::fd::AsRawFd;
        let perm = if mode & 0o111 != 0 {
            PERM_READ | PERM_EXEC
        } else if mode & 0o200 == 0 {
            PERM_READ
        } else {
            return Ok(());
        };
        moto_rt::fs::set_file_perm(file.as_raw_fd(), perm).map_err(motor_error)?;
    }
    #[cfg(not(target_os = "motor"))]
    let _ = (file, mode);
    Ok(())
}

/// Creates a directory for an extracted member; it stays writable by its
/// owner so that its contents can be extracted.
pub fn create_dir(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode((mode | 0o700) & 0o777)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        fs::create_dir(path)
    }
}

#[cfg(target_os = "motor")]
fn motor_error(error: moto_rt::Error) -> io::Error {
    io::Error::from_raw_os_error(moto_rt::ErrorCode::from(error).into())
}
