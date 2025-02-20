/*
 * Copyright © 2018, Steve Smith <tarkasteve@gmail.com>
 *
 * This program is free software: you can redistribute it and/or
 * modify it under the terms of the GNU General Public License version
 * 3 as published by the Free Software Foundation.
 *
 * This program is distributed in the hope that it will be useful, but
 * WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use libc;
use std::cmp;
use std::io;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::AsRawFd;
use std::mem;

use crate::errors::{Result, XcpError};

pub fn result_or_errno<T>(result: i64, retval: T) -> Result<T> {
    match result {
        -1 => Err(io::Error::last_os_error().into()),
        _ => Ok(retval),
    }
}


const BLKSIZE: usize = 4 * 1024;  // Assume 4k blocks on disk.
type Buffer = [u8; BLKSIZE];

fn get_buffer() -> Buffer {
    let buf: Buffer = unsafe {
        mem::MaybeUninit::uninit().assume_init()
    };
    buf
}

fn pread(fd: &File, buf: &mut [u8], nbytes: usize, off: usize) -> Result<usize> {
    let ret = unsafe {
        libc::pread(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, nbytes, off as i64)
    };

    result_or_errno(ret as i64, ret as usize)
}

fn pwrite(fd: &File, buf: &mut [u8], nbytes: usize, off: usize) -> Result<usize> {
    let ret = unsafe {
        libc::pwrite(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, nbytes, off as i64)
    };

    result_or_errno(ret as i64, ret as usize)
}

#[allow(dead_code)]
pub fn copy_range_uspace(reader: &File, writer: &File, nbytes: usize, off: usize) -> Result<u64> {
    let mut buf = get_buffer();

    let mut written: usize = 0;
    while written < nbytes {
        let next = cmp::min(nbytes - written, BLKSIZE);
        let noff = off + written;

        let rlen = match pread(reader, &mut buf[..next], next, noff) {
            Ok(0) => return Err(XcpError::InvalidSource { msg: "Source file ended prematurely."}.into()),
            Ok(len) => len,
            Err(e) => return Err(e),
        };

        let _wlen = match pwrite(writer, &mut buf[..rlen], next, noff) {
            Ok(len) if len < rlen => return Err(XcpError::InvalidSource { msg: "Failed write to file."}.into()),
            Ok(len) => len,
            Err(e) => return Err(e),
        };

        written += rlen;
    }
    Ok(written as u64)
}


// Slightly modified version of io::copy() that only copies a set amount of bytes.
pub fn copy_bytes_uspace(mut reader: &File, mut writer: &File, nbytes: usize) -> Result<u64> {
    let mut buf = get_buffer();

    let mut written = 0;
    while written < nbytes {
        let next = cmp::min(nbytes - written, BLKSIZE);
        let len = match reader.read(&mut buf[..next]) {
            Ok(0) => return Err(XcpError::InvalidSource { msg: "Source file ended prematurely."}.into()),
            Ok(len) => len,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(XcpError::IOError { err: e}.into()),
        };
        writer.write_all(&buf[..len])?;
        written += len;
    }
    Ok(written as u64)
}


/// Version of copy_file_range that defers offset-management to the
/// syscall. see copy_file_range(2) for details.
#[allow(dead_code)]
pub fn copy_file_bytes(infd: &File, outfd: &File, bytes: u64) -> Result<u64> {
    Ok(copy_bytes_uspace(infd, outfd, bytes as usize)?)
}

// Copy a single file block.
// TODO: Not used currently, intended for parallel block copy support.
#[allow(dead_code)]
pub fn copy_file_offset(infd: &File, outfd: &File, bytes: u64, off: i64) -> Result<u64> {
    copy_range_uspace(infd, outfd, bytes as usize, off as usize)
}


// No sparse file handling by default, needs to be implemented
// per-OS. This effectively disables the following operations.
#[allow(dead_code)]
pub fn probably_sparse(_fd: &File) -> Result<bool> {
    Ok(false)
}

#[allow(dead_code)]
pub fn allocate_file(_fd: &File, _len: u64) -> Result<()> {
    Err(XcpError::UnsupportedOperation {}.into())
}

#[allow(dead_code)]
pub fn next_sparse_segments(_infd: &File, _outfd: &File, _pos: u64) -> Result<(u64, u64)> {
    Err(XcpError::UnsupportedOperation {}.into())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::iter;
    use std::fs::read;
    use tempfile::tempdir;

    #[test]
    fn test_copy_bytes_uspace_large() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("from.bin");
        let to = dir.path().join("to.bin");
        let size = 128*1024;
        let data = iter::repeat("X").take(size).collect::<String>();

        {
            let mut fd: File = File::create(&from).unwrap();
            write!(fd, "{}", data).unwrap();
        }

        {
            let infd = File::open(&from).unwrap();
            let outfd = File::create(&to).unwrap();
            let written = copy_bytes_uspace(&infd, &outfd, size).unwrap();

            assert_eq!(written, size as u64);
        }

        assert_eq!(from.metadata().unwrap().len(),
                   to.metadata().unwrap().len());

        {
            let from_data = read(&from).unwrap();
            let to_data = read(&to).unwrap();
            assert_eq!(from_data, to_data);
        }
    }


    #[test]
    fn test_copy_range_uspace_large() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("from.bin");
        let to = dir.path().join("to.bin");
        let size = 128*1024;
        let data = iter::repeat("X").take(size).collect::<String>();

        {
            let mut fd: File = File::create(&from).unwrap();
            write!(fd, "{}", data).unwrap();
        }


        {
            let infd = File::open(&from).unwrap();
            let outfd = File::create(&to).unwrap();

            let blocksize = size/4;
            let mut written = 0;

            for off in (0..4).rev() {
                written += copy_range_uspace(&infd, &outfd, blocksize, blocksize * off).unwrap();
            }

            assert_eq!(written, size as u64);
        }

        assert_eq!(from.metadata().unwrap().len(),
                   to.metadata().unwrap().len());

        {
            let from_data = read(&from).unwrap();
            let to_data = read(&to).unwrap();
            assert_eq!(from_data, to_data);
        }
    }

}
