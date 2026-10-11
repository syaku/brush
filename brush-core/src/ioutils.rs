//! Internal I/O utilities.

use crate::openfiles;

/// Error type for `FailingReaderWriter`.
#[derive(Clone, Debug, thiserror::Error)]
enum ReaderWriterError {
    /// I/O read error.
    #[error("I/O read error: {0}")]
    Read(&'static str),
    /// I/O write error.
    #[error("I/O write error: {0}")]
    Write(&'static str),
    /// I/O flush error.
    #[error("I/O flush error: {0}")]
    Flush(&'static str),
}

/// An implementation of `std::io::Read` and `std::io::Write` that always fails.
///
/// Also implements `openfiles::Stream` for use with `openfiles::OpenFile`. Useful
/// for providing a valid stream that gracefully fails all operations in cases where
/// no real I/O is possible.
#[derive(Clone)]
pub(crate) struct FailingReaderWriter {
    message: &'static str,
}

impl FailingReaderWriter {
    /// Creates a new `FailingReaderWriter` with the given error message.
    ///
    /// # Arguments
    ///
    /// * `message` - The error message to use for all operations.
    pub const fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl openfiles::Stream for FailingReaderWriter {
    fn clone_box(&self) -> Box<dyn openfiles::Stream> {
        Box::new(self.clone())
    }

    #[cfg(unix)] // ast-grep-ignore: platform-cfg-outside-sys
    fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, crate::error::Error> {
        Err(crate::error::ErrorKind::CannotConvertToNativeFd.into())
    }

    #[cfg(unix)] // ast-grep-ignore: platform-cfg-outside-sys
    fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, crate::error::Error> {
        Err(crate::error::ErrorKind::CannotConvertToNativeFd.into())
    }
}

impl std::io::Read for FailingReaderWriter {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(ReaderWriterError::Read(self.message)))
    }
}

impl std::io::Write for FailingReaderWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(ReaderWriterError::Write(
            self.message,
        )))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other(ReaderWriterError::Flush(
            self.message,
        )))
    }
}

impl From<FailingReaderWriter> for openfiles::OpenFile {
    fn from(frw: FailingReaderWriter) -> Self {
        Self::Stream(Box::new(frw))
    }
}

/// Reads shell input for the parser, optionally reading each `\r\n` as `\n`.
///
/// A `\r` that ends the input is read as a line ending too: `$(...)` strips the final `\n`
/// from `\r\n`-terminated output, so `eval "$(tool init bash)"` sees one there.
///
/// Input is read as it arrives rather than all at once, so a script read from a pipe
/// starts running before the writer finishes.
pub(crate) struct ShellInputReader<R> {
    inner: std::io::BufReader<R>,
    crlf_as_lf: bool,
}

impl<R: std::io::Read> ShellInputReader<R> {
    /// Creates a reader over `inner`.
    ///
    /// # Arguments
    ///
    /// * `inner` - The input to read.
    /// * `crlf_as_lf` - Whether to drop each `\r` that immediately precedes a `\n` or ends
    ///   the input.
    pub fn new(inner: R, crlf_as_lf: bool) -> Self {
        Self {
            inner: std::io::BufReader::new(inner),
            crlf_as_lf,
        }
    }
}

impl<R: std::io::Read> std::io::Read for ShellInputReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::BufRead as _;

        if !self.crlf_as_lf || buf.is_empty() {
            return self.inner.read(buf);
        }

        loop {
            let available = self.inner.fill_buf()?;
            if available.is_empty() {
                return Ok(0);
            }

            let mut used = 0;
            let mut written = 0;
            while used < available.len() && written < buf.len() {
                let byte = available[used];
                if byte == b'\r' {
                    match available.get(used + 1) {
                        Some(b'\n') => {
                            used += 1;
                            continue;
                        }
                        // What follows the `\r` hasn't arrived yet.
                        None => break,
                        Some(_) => {}
                    }
                }
                buf[written] = byte;
                written += 1;
                used += 1;
            }
            self.inner.consume(used);

            if written > 0 {
                return Ok(written);
            }

            // Nothing was written, so the buffer held only a `\r`. Only now, with nothing
            // to hand back, is it worth waiting on the input for what follows it.
            self.inner.consume(1);
            match self.inner.fill_buf()?.first() {
                // The `\n` is handed back on the next pass; at the end, there's nothing more.
                Some(b'\n') | None => {}
                Some(_) => {
                    buf[0] = b'\r';
                    return Ok(1);
                }
            }
        }
    }
}

#[cfg(test)]
#[expect(clippy::panic_in_result_fn)]
mod tests {
    use std::io::Read as _;

    use super::*;

    /// Hands out its input one byte per read, so every `\r` ends a read.
    struct OneByteAtATime<'a>(&'a [u8]);

    impl std::io::Read for OneByteAtATime<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match (self.0.split_first(), buf.first_mut()) {
                (Some((byte, rest)), Some(slot)) => {
                    *slot = *byte;
                    self.0 = rest;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    fn read_all(reader: impl std::io::Read, crlf_as_lf: bool) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        ShellInputReader::new(reader, crlf_as_lf).read_to_end(&mut out)?;
        Ok(out)
    }

    const INPUT: &[u8] = b"f() {\r\n  echo 'a\rb'\r\n}\r\r\nfi\r";
    const CRLF_AS_LF: &[u8] = b"f() {\n  echo 'a\rb'\n}\r\nfi";

    #[test]
    fn crlf_is_read_as_lf() -> std::io::Result<()> {
        assert_eq!(read_all(INPUT, true)?, CRLF_AS_LF);
        Ok(())
    }

    #[test]
    fn crlf_split_across_reads_is_read_as_lf() -> std::io::Result<()> {
        assert_eq!(read_all(OneByteAtATime(INPUT), true)?, CRLF_AS_LF);
        Ok(())
    }

    #[test]
    fn input_is_unchanged_when_disabled() -> std::io::Result<()> {
        assert_eq!(read_all(OneByteAtATime(INPUT), false)?, INPUT);
        Ok(())
    }
}
