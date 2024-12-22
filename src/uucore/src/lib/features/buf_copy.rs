// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! This module provides several buffer-based copy/write functions that leverage
//! the `splice` system call in Linux systems, thus increasing the I/O
//! performance of copying between two file descriptors. This module is mostly
//! used by utilities to work around the limitations of Rust's `fs::copy` which
//! does not handle copying special files (e.g pipes, character/block devices).

use crate::error::{UError, UResult};
use nix::unistd;
use std::fs::File;
use std::{
    io::{self, Read, Write},
    os::{
        fd::AsFd,
        unix::io::{AsRawFd, RawFd},
    },
};

use nix::{errno::Errno, libc::S_IFIFO, sys::stat::fstat};

use super::pipes::{pipe, splice, splice_exact, vmsplice};

type Result<T> = std::result::Result<T, Error>;

/// Error types used by buffer-copying functions from the `buf_copy` module.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    WriteError(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::WriteError(msg) => write!(f, "splice() write error: {}", msg),
            Error::Io(err) => write!(f, "I/O error: {}", err),
        }
    }
}

impl std::error::Error for Error {}

impl UError for Error {
    fn code(&self) -> i32 {
        1
    }

    fn usage(&self) -> bool {
        false
    }
}

/// Helper function to determine whether a given handle (such as a file) is a pipe or not.
///
/// # Arguments
/// * `out` - path of handle
///
/// # Returns
/// A `bool` indicating whether the given handle is a pipe or not.
#[inline]
#[cfg(unix)]
pub fn is_pipe<P>(path: &P) -> Result<bool>
where
    P: AsRawFd,
{
    Ok(fstat(path.as_raw_fd())?.st_mode as nix::libc::mode_t & S_IFIFO != 0)
}

const SPLICE_SIZE: usize = 1024 * 128;
const BUF_SIZE: usize = 1024 * 16;

/// Copy data from `Read` implementor `source` into a `Write` implementor
/// `dest`. This works by reading a chunk of data from `source` and writing the
/// data to `dest` in a loop.
///
/// This function uses the Linux-specific `splice` call when possible which does
/// not use any intermediate user-space buffer. It falls backs to
/// `std::io::copy` under other platforms or when the call fails and is still
/// recoverable.
///
/// # Arguments
/// * `source` - `Read` implementor to copy data from.
/// * `dest` - `Write` implementor to copy data to.
///
/// # Returns
///
/// Result of operation and bytes successfully written (as a `u64`) when
/// operation is successful.
pub fn copy_stream<R, S>(src: &mut R, dest: &mut S) -> UResult<u64>
where
    R: Read + AsFd + AsRawFd,
    S: Write + AsFd + AsRawFd,
{
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // If we're on Linux or Android, try to use the splice() system call
        // for faster writing. If it works, we're done.
        let result = splice_write(src, &dest.as_fd())?;
        if !result.1 {
            return Ok(result.0);
        }
    }
    // If we're not on Linux or Android, or the splice() call failed,
    // fall back on slower writing.
    let result = std::io::copy(src, dest)?;

    // If the splice() call failed and there has been some data written to
    // stdout via while loop above AND there will be second splice() call
    // that will succeed, data pushed through splice will be output before
    // the data buffered in stdout.lock. Therefore additional explicit flush
    // is required here.
    dest.flush()?;
    Ok(result)
}

/// Write from source `handle` into destination `write_fd` using Linux-specific
/// `splice` system call.
///
/// # Arguments
/// - `source` - source handle
/// - `dest` - destination handle
#[inline]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn splice_write<R, S>(source: &R, dest: &S) -> UResult<(u64, bool)>
where
    R: Read + AsFd + AsRawFd,
    S: AsRawFd + AsFd,
{
    let (pipe_rd, pipe_wr) = pipe()?;
    let mut bytes: u64 = 0;

    loop {
        match splice(&source, &pipe_wr, SPLICE_SIZE) {
            Ok(n) => {
                if n == 0 {
                    return Ok((bytes, false));
                }
                if splice_exact(&pipe_rd, dest, n).is_err() {
                    // If the first splice manages to copy to the intermediate
                    // pipe, but the second splice to stdout fails for some reason
                    // we can recover by copying the data that we have from the
                    // intermediate pipe to stdout using normal read/write. Then
                    // we tell the caller to fall back.
                    copy_exact(pipe_rd.as_raw_fd(), dest, n)?;
                    return Ok((bytes, true));
                }

                bytes += n as u64;
            }
            Err(_) => {
                return Ok((bytes, true));
            }
        }
    }
}

/// Move exactly `num_bytes` bytes from `read_fd` to `write_fd` using the `read`
/// and `write` calls.
fn copy_exact(read_fd: RawFd, write_fd: &impl AsFd, num_bytes: usize) -> std::io::Result<usize> {
    let mut left = num_bytes;
    let mut buf = [0; BUF_SIZE];
    let mut written = 0;
    while left > 0 {
        let read = unistd::read(read_fd, &mut buf)?;
        assert_ne!(read, 0, "unexpected end of pipe");
        while written < read {
            let n = unistd::write(write_fd, &buf[written..read])?;
            written += n;
        }
        left -= read;
    }
    Ok(written)
}

/// Write input `bytes` to a file descriptor. This uses the Linux-specific
/// `vmsplice()` call to write into a file descriptor directly, which only works
/// if the destination is a pipe.
///
/// # Arguments
/// * `bytes` - data to be written
/// * `dest` - destination handle
///
/// # Returns
/// When write succeeds, the amount of bytes written is returned as a
/// `u64`. The `bool` indicates if we need to fall back to normal copying or
/// not. `true` means we need to fall back, `false` means we don't have to.
///
/// A `UError` error is returned when the operation is not supported or when an
/// I/O error occurs.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn splice_data_to_pipe<T>(bytes: &[u8], dest: &T) -> UResult<(u64, bool)>
where
    T: AsRawFd + AsFd,
{
    let mut n_bytes: u64 = 0;
    let mut bytes = bytes;
    while !bytes.is_empty() {
        let len = match vmsplice(dest, bytes) {
            Ok(n) => n,
            // The maybe_unsupported call below may emit an error, when the
            // error is considered as unrecoverable error (ones that won't make
            // us fall back to other method)
            Err(e) => return Ok(maybe_unsupported(e)?),
        };
        bytes = &bytes[len..];
        n_bytes += len as u64;
    }
    Ok((n_bytes, false))
}

/// Write input `bytes` to a handle using a temporary pipe. A `vmsplice()` call
/// is issued to write to the temporary pipe, which then gets written to the
/// final destination using `splice()`.
///
/// # Arguments * `bytes` - data to be written * `dest` - destination handle
///
/// # Returns When write succeeds, the amount of bytes written is returned as a
/// `u64`. The `bool` indicates if we need to fall back to normal copying or
/// not. `true` means we need to fall back, `false` means we don't have to.
///
/// A `UError` error is returned when the operation is not supported or when an
/// I/O error occurs.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn splice_data_to_fd<T: AsFd>(
    bytes: &[u8],
    read_pipe: &File,
    write_pipe: &File,
    dest: &T,
) -> UResult<(u64, bool)> {
    loop {
        let mut bytes = bytes;
        while !bytes.is_empty() {
            let len = match vmsplice(&write_pipe, bytes) {
                Ok(n) => n,
                Err(e) => return Ok(maybe_unsupported(e)?),
            };
            if let Err(e) = splice_exact(&read_pipe, dest, len) {
                return Ok(maybe_unsupported(e)?);
            }
            bytes = &bytes[len..];
        }
    }
}

/// Conversion from a `nix::Error` into our `Error` which implements `UError`.
#[cfg(any(target_os = "linux", target_os = "android"))]
impl From<nix::Error> for Error {
    fn from(error: nix::Error) -> Self {
        Self::Io(io::Error::from_raw_os_error(error as i32))
    }
}

/// Several error values from `nix::Error` (`EINVAL`, `ENOSYS`, and `EBADF`) get
/// treated as errors indicating that the `splice` call is not available, i.e we
/// can still recover from the error. Thus, return the final result of the call
/// as `Result` and indicate that we have to fall back using other write method.
///
/// # Arguments
/// * `error` - the `nix::Error` received
///
/// # Returns
/// Result with tuple containing a `u64` `0` indicating that no data had been
/// written and a `true` indicating we have to fall back, if error is still
/// recoverable. Returns an `Error` implementing `UError` otherwise.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn maybe_unsupported(error: nix::Error) -> Result<(u64, bool)> {
    match error {
        Errno::EINVAL | Errno::ENOSYS | Errno::EBADF => Ok((0, true)),
        _ => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::pipes;

    fn new_temp_file() -> File {
        let temp_dir = tempdir().unwrap();
        File::create(temp_dir.path().join("file.txt")).unwrap()
    }

    #[test]
    fn test_file_is_pipe() {
        let temp_file = new_temp_file();
        let (pipe_read, pipe_write) = pipes::pipe().unwrap();

        assert!(is_pipe(&pipe_read).unwrap());
        assert!(is_pipe(&pipe_write).unwrap());
        assert!(!is_pipe(&temp_file).unwrap());
    }

    #[test]
    fn test_valid_splice_errs() {
        let err = nix::Error::from(Errno::EINVAL);
        assert_eq!(maybe_unsupported(err).unwrap(), (0, true));

        let err = nix::Error::from(Errno::ENOSYS);
        assert_eq!(maybe_unsupported(err).unwrap(), (0, true));

        let err = nix::Error::from(Errno::EBADF);
        assert_eq!(maybe_unsupported(err).unwrap(), (0, true));

        let err = nix::Error::from(Errno::EPERM);
        assert!(maybe_unsupported(err).is_err());
    }

    #[test]
    fn test_splice_data_to_pipe() {
        let (pipe_read, pipe_write) = pipes::pipe().unwrap();
        let data = b"Hello, world!";
        let (bytes, _) = splice_data_to_pipe(data, &pipe_write).unwrap();
        let mut buf = [0; 1024];
        let n = unistd::read(pipe_read.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(&buf[..n], data);
        assert_eq!(bytes as usize, data.len());
    }

    #[test]
    fn test_splice_data_to_file() {
        let mut temp_file = new_temp_file();
        let (pipe_read, pipe_write) = pipes::pipe().unwrap();
        let data = b"Hello, world!";
        let (bytes, _) = splice_data_to_fd(data, &pipe_read, &pipe_write, &temp_file).unwrap();
        let mut buf = [0; 1024];
        let n = temp_file.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], data);
        assert_eq!(bytes as usize, data.len());
    }

    #[test]
    fn test_copy_exact() {
        let (mut pipe_read, mut pipe_write) = pipes::pipe().unwrap();
        let data = b"Hello, world!";
        let n = pipe_write.write(data).unwrap();
        assert_eq!(n, data.len());
        let mut buf = [0; 1024];
        let n = copy_exact(pipe_read.as_raw_fd(), &pipe_write, data.len()).unwrap();
        let n2 = pipe_read.read(&mut buf).unwrap();
        assert_eq!(n, n2);
        assert_eq!(&buf[..n], data);
    }

    #[test]
    fn test_copy_stream() {
        let (mut pipe_read, mut pipe_write) = pipes::pipe().unwrap();
        let data = b"Hello, world!";
        let n = pipe_write.write(data).unwrap();
        assert_eq!(n, data.len());
        let mut buf = [0; 1024];
        let n = copy_stream(&mut pipe_read, &mut pipe_write).unwrap();
        let n2 = pipe_read.read(&mut buf).unwrap();
        assert_eq!(n as usize, n2);
        assert_eq!(&buf[..n as usize], data);
    }

    #[test]
    fn test_splice_write() {
        let (mut pipe_read, pipe_write) = pipes::pipe().unwrap();
        let data = b"Hello, world!";
        let (bytes, _) = splice_write(&pipe_read, &pipe_write).unwrap();
        let mut buf = [0; 1024];
        let n = pipe_read.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], data);
        assert_eq!(bytes as usize, data.len());
    }
}