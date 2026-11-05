//! The daemon's log file, followed across rotations.
//!
//! launchd opens the log (`StandardOutPath`) once for the daemon. When
//! newsyslog rotates it, it renames `ovsc.log` to `ovsc.log.0` and creates
//! an empty `ovsc.log`, but the daemon's output still goes to the renamed
//! file: `ovsc.log` stays empty, so newsyslog never rotates again and
//! `ovsc.log.0` grows for as long as the daemon runs.
//!
//! With `--log-file` the daemon opens the log itself, in append mode, as
//! its standard output and error (so panics land there too), and looks
//! every few seconds whether the path still names the file it writes to.
//! Once it does not (renamed or removed), it opens the path again. No
//! signal or pid file is needed: the lines logged between the rotation and
//! the next look end up at the end of the rotated file.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{info, warn};

/// How often the daemon looks whether its log was rotated.
pub const FOLLOW_EVERY: Duration = Duration::from_secs(5);

/// A log file that some of this process's file descriptors write to.
pub struct LogFile {
    path: PathBuf,
    /// The descriptors redirected to it: stdout and stderr in the daemon.
    #[cfg(unix)]
    fds: Vec<std::os::fd::RawFd>,
    /// The file they write to now.
    file: File,
}

impl LogFile {
    /// Sends this process's standard output and error to the end of the
    /// file at `path`, created if need be.
    #[cfg(unix)]
    pub fn stdout_and_stderr(path: &Path) -> io::Result<LogFile> {
        Self::redirect(path, vec![libc::STDOUT_FILENO, libc::STDERR_FILENO])
    }

    #[cfg(not(unix))]
    pub fn stdout_and_stderr(_path: &Path) -> io::Result<LogFile> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "--log-file needs a Unix system"))
    }

    /// Sends `fds` to the end of the file at `path`, created if need be.
    #[cfg(unix)]
    fn redirect(path: &Path, fds: Vec<std::os::fd::RawFd>) -> io::Result<LogFile> {
        let file = open(path)?;
        let log = LogFile { path: path.to_owned(), fds, file };
        log.point_fds()?;
        Ok(log)
    }

    /// Points every redirected descriptor at `self.file`.
    #[cfg(unix)]
    fn point_fds(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        for &fd in &self.fds {
            // SAFETY: both are open descriptors; dup2 closes `fd`'s old
            // file and points `fd` at ours, atomically.
            if unsafe { libc::dup2(self.file.as_raw_fd(), fd) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Whether the path no longer names the file being written.
    #[cfg(unix)]
    fn rotated(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        let (Ok(now), Ok(ours)) = (std::fs::metadata(&self.path), self.file.metadata()) else {
            return true;
        };
        (now.dev(), now.ino()) != (ours.dev(), ours.ino())
    }

    /// Opens the path again if the file was rotated away; whether it was.
    #[cfg(unix)]
    pub fn follow(&mut self) -> io::Result<bool> {
        if !self.rotated() {
            return Ok(false);
        }
        self.file = open(&self.path)?;
        self.point_fds()?;
        Ok(true)
    }

    #[cfg(not(unix))]
    pub fn follow(&mut self) -> io::Result<bool> {
        Ok(false)
    }

    /// Follows the log across rotations, for as long as the daemon runs.
    pub async fn keep(mut self) {
        let mut tick = tokio::time::interval(FOLLOW_EVERY);
        let mut warned = false;
        loop {
            tick.tick().await;
            match self.follow() {
                Ok(reopened) => {
                    if reopened {
                        info!("log file {} reopened after a rotation", self.path.display());
                    }
                    warned = false;
                }
                // Once: the old file still takes the lines meanwhile.
                Err(e) => {
                    if !warned {
                        warn!("cannot reopen the log {}: {e}", self.path.display());
                    }
                    warned = true;
                }
            }
        }
    }
}

/// Opens `path` for appending, readable by everyone like newsyslog's.
fn open(path: &Path) -> io::Result<File> {
    let mut o = File::options();
    o.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o644);
    o.open(path)
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn the_log_is_followed_across_a_rotation() {
        let dir = std::env::temp_dir().join(format!("ovsc-logfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ovsc.log");
        // Stands in for stdout: a descriptor of our own, redirected.
        let mut out = File::create(dir.join("elsewhere")).unwrap();
        let mut log = LogFile::redirect(&path, vec![out.as_raw_fd()]).unwrap();
        writeln!(out, "one").unwrap();
        assert!(!log.follow().unwrap(), "nothing to follow yet");

        // newsyslog: rename, then a fresh empty file.
        std::fs::rename(&path, dir.join("ovsc.log.0")).unwrap();
        File::create(&path).unwrap();
        writeln!(out, "two").unwrap();
        assert!(log.follow().unwrap());
        writeln!(out, "three").unwrap();
        assert!(!log.follow().unwrap());

        // Removed outright: created again.
        std::fs::remove_file(&path).unwrap();
        assert!(log.follow().unwrap());
        writeln!(out, "four").unwrap();

        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(read("ovsc.log.0"), "one\ntwo\n");
        assert_eq!(read("ovsc.log"), "four\n");
        assert_eq!(read("elsewhere"), "");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
