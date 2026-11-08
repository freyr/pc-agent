//! The log file of the Windows service: one file with a size limit and one
//! old file. A write that would take the file above the limit first renames
//! it to `<name>.1` (which replaces the old file) and starts a new file.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub struct LogFile {
    path: PathBuf,
    limit: u64,
    file: Option<File>,
    size: u64,
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl LogFile {
    /// Opens `path` for append; the size of an existing file counts.
    pub fn open(path: &Path, limit: u64) -> io::Result<Self> {
        let file = open_append(path)?;
        let size = file.metadata()?.len();
        Ok(LogFile {
            path: path.to_path_buf(),
            limit,
            file: Some(file),
            size,
        })
    }

    /// The path of the old file: `<path>.1`.
    pub fn old_path(&self) -> PathBuf {
        let mut name = OsString::from(self.path.as_os_str());
        name.push(".1");
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Windows cannot rename an open file.
        self.file = None;
        let renamed = std::fs::rename(&self.path, self.old_path());
        let file = open_append(&self.path)?;
        self.size = file.metadata()?.len();
        self.file = Some(file);
        renamed
    }
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.limit {
            self.rotate()?;
        }
        let file = match &mut self.file {
            Some(file) => file,
            None => self.file.insert(open_append(&self.path)?),
        };
        let n = file.write(buf)?;
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new empty directory for one test.
    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pc-agent-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_write_above_the_limit_moves_the_file_to_the_one_old_file() {
        let dir = dir("rotate");
        let path = dir.join("pc-agent.log");
        let mut log = LogFile::open(&path, 10).unwrap();
        log.write_all(b"aaaaaa\n").unwrap();
        log.write_all(b"bbbbbb\n").unwrap();
        assert_eq!(read(&path), "bbbbbb\n");
        assert_eq!(read(&dir.join("pc-agent.log.1")), "aaaaaa\n");
        log.write_all(b"cccccc\n").unwrap();
        assert_eq!(read(&path), "cccccc\n");
        assert_eq!(read(&dir.join("pc-agent.log.1")), "bbbbbb\n");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        // Windows keeps the folder while a file in it is open.
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_up_to_the_limit_stay_in_one_file() {
        let dir = dir("fill");
        let path = dir.join("pc-agent.log");
        let mut log = LogFile::open(&path, 10).unwrap();
        log.write_all(b"aaaaa").unwrap();
        log.write_all(b"bbbbb").unwrap();
        assert_eq!(read(&path), "aaaaabbbbb");
        assert!(!dir.join("pc-agent.log.1").exists());
        // A line longer than the limit goes into an empty file.
        let big = dir.join("big.log");
        let mut big_log = LogFile::open(&big, 4).unwrap();
        big_log.write_all(b"0123456789").unwrap();
        assert_eq!(read(&big), "0123456789");
        assert!(!dir.join("big.log.1").exists());
        drop((log, big_log));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_size_of_an_existing_file_counts() {
        let dir = dir("reopen");
        let path = dir.join("pc-agent.log");
        std::fs::write(&path, "aaaaaaaa\n").unwrap();
        let mut log = LogFile::open(&path, 10).unwrap();
        log.write_all(b"bb\n").unwrap();
        assert_eq!(read(&path), "bb\n");
        assert_eq!(read(&dir.join("pc-agent.log.1")), "aaaaaaaa\n");
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
