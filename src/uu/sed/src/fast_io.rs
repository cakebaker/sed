// Zero-copy line-based I/O
//
// Abstractions that allow file lines to be processed and output
// in mmapped memory space.  By coallescing output requests an
// efficient write(2) system call can be issued for them, bypassing
// the copy required for output through BufWriter.
// Search for "main" to see a usage example.
//
// SPDX-License-Identifier: MIT
// Copyright (c) 2025 Diomidis Spinellis
//
// This file is part of the uutils sed package.
// It is licensed under the MIT License.
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

#[cfg(unix)]
use memchr::memchr;
#[cfg(unix)]
use memmap2::Mmap;

use std::cell::Cell;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};

#[cfg(not(unix))]
use std::marker::PhantomData;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use std::path::PathBuf;
use std::str;

#[cfg(unix)]
use uucore::libc::{c_void, write};

use uucore::error::UError;

#[cfg(unix)]
use uucore::error::USimpleError;

// Define two cursors for iterating over lines:
// - MmapLineCursor based on mmap(2),
// - ReadLineCursorbased on BufReader.

/// Cursor for zero-copy iteration over mmap’d file.
#[cfg(unix)]
pub struct MmapLineCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

#[cfg(unix)]
/// Represents the get_line return: one line plus whether it was the last.
pub struct NextMmapLine<'a> {
    pub content: &'a [u8],
    pub full_span: &'a [u8],
    pub is_last_line: bool,
}

#[cfg(unix)]
impl<'a> MmapLineCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Return the next line, if available, or None.
    fn get_line(&mut self) -> io::Result<Option<NextMmapLine<'_>>> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }

        let start = self.pos;

        let mut end = if let Some(pos) = memchr(b'\n', &self.data[start..]) {
            pos + start
        } else {
            self.data.len()
        };

        if end < self.data.len() {
            end += 1; // include \n in full span
        }

        self.pos = end;
        let full_span = &self.data[start..end];
        let content = if full_span.ends_with(b"\n") {
            &full_span[..full_span.len() - 1]
        } else {
            full_span
        };

        let is_last_line = self.pos >= self.data.len();
        Ok(Some(NextMmapLine {
            content,
            full_span,
            is_last_line,
        }))
    }
}

/// Buffered line reader from any BufRead input.
pub struct ReadLineCursor {
    reader: Box<dyn BufRead>,
    buffer: String,
}

impl ReadLineCursor {
    /// Construct from anything that implements `Read`.
    fn new<R: Read + 'static>(r: R) -> Self {
        let buf = BufReader::new(r);
        Self {
            reader: Box::new(buf),
            buffer: String::new(),
        }
    }

    /// If a line is available, return it, its \n termination,
    /// and next line availability, itherwise return None.
    fn get_line(&mut self) -> io::Result<Option<(String, bool, bool)>> {
        self.buffer.clear();
        // read_line *includes* the '\n' if present
        let bytes_read = self.reader.read_line(&mut self.buffer)?;
        if bytes_read == 0 {
            return Ok(None);
        }
        // O(1) check whether it ended in '\n'
        let has_newline = self.buffer.ends_with('\n');
        // strip it if you don’t want to expose it to the caller
        if has_newline {
            self.buffer.pop();
        }
        let line = std::mem::take(&mut self.buffer);
        let is_last_line = self.reader.fill_buf()?.is_empty();
        Ok(Some((line, has_newline, is_last_line)))
    }
}

/// As chunk of data that is input and can be output, often very efficiently
#[derive(Debug, PartialEq, Eq)]
pub struct IOChunk<'a> {
    utf8_verified: Cell<bool>, // True if the contents are valid UTF-8
    content: IOChunkContent<'a>,
}

impl<'a> IOChunk<'a> {
    /// Construct an IOChunk from the given content
    fn from_content(content: IOChunkContent<'a>) -> Self {
        Self {
            utf8_verified: Cell::new(false),
            content,
        }
    }

    /// Clear the object's contents, converting it into Owned if needed.
    pub fn clear(&mut self) {
        self.utf8_verified.set(true);
        match &mut self.content {
            IOChunkContent::Owned {
                content,
                has_newline,
                ..
            } => {
                content.clear();
                *has_newline = false;
            }
            #[cfg(unix)]
            _ => {
                self.content = IOChunkContent::new_owned(String::new(), false);
            }
        }
    }

    /// Return true if the content is empty.
    pub fn is_empty(&self) -> bool {
        self.content.len() == 0
    }

    /// Return true if the content ends with a newline.
    pub fn is_newline_terminated(&self) -> bool {
        match &self.content {
            IOChunkContent::Owned { has_newline, .. } => *has_newline,
            #[cfg(unix)]
            IOChunkContent::MmapInput { full_span, .. } => {
                if let Some(&last) = full_span.last() {
                    last == b'\n'
                } else {
                    false
                }
            }
        }
    }

    #[cfg(test)]
    /// Create an Owned newline-terminated IOChunk from a string.
    pub fn from_str(s: &str) -> Self {
        IOChunk {
            content: IOChunkContent::new_owned(s.to_string(), true),
            utf8_verified: Cell::new(false),
        }
    }

    /// Set the object's contents to the specified string.
    /// Convert it into Owned if needed.
    pub fn set_to_string(&mut self, new_content: String, add_newline: bool) {
        self.utf8_verified.set(true);
        match &mut self.content {
            IOChunkContent::Owned {
                content,
                has_newline,
                ..
            } => {
                *content = new_content;
                *has_newline = add_newline;
            }
            #[cfg(unix)]
            _ => {
                self.content = IOChunkContent::new_owned(new_content, add_newline);
            }
        }
    }

    /// Return the content as a str.
    pub fn as_str(&self) -> Result<&str, Box<dyn UError>> {
        match &self.content {
            #[cfg(unix)]
            IOChunkContent::MmapInput { content, .. } => {
                if self.utf8_verified.get() {
                    // Use cached result
                    Ok(unsafe { self.content.as_str_unchecked() })
                } else {
                    let result = str::from_utf8(content);
                    self.utf8_verified.set(true);
                    result.map_err(|e| USimpleError::new(2, e.to_string()))
                }
            }
            IOChunkContent::Owned { content, .. } => Ok(content),
        }
    }

    /// Return the raw byte content (always safe).
    pub fn as_bytes(&self) -> &[u8] {
        match &self.content {
            #[cfg(unix)]
            IOChunkContent::MmapInput { content, .. } => content,
            IOChunkContent::Owned { content, .. } => content.as_bytes(),
        }
    }

    /// Convert content to the Owned variant if it's not already.
    /// Fails if the conversion to UTF-8 fails.
    pub fn ensure_owned(&mut self) -> Result<(), Box<dyn UError>> {
        match &self.content {
            IOChunkContent::Owned { .. } => Ok(()), // already owned
            #[cfg(unix)]
            IOChunkContent::MmapInput { content, full_span } => {
                match std::str::from_utf8(content) {
                    Ok(valid_str) => {
                        let has_newline = full_span.last().copied() == Some(b'\n');
                        self.content =
                            IOChunkContent::new_owned(valid_str.to_string(), has_newline);
                        self.utf8_verified.set(true);
                        Ok(())
                    }
                    Err(e) => Err(USimpleError::new(2, e.to_string())),
                }
            }
        }
    }

    /// Return mutable access to the content and has_newline fields.
    pub fn fields_mut(&mut self) -> Result<(&mut String, &mut bool), Box<dyn UError>> {
        self.ensure_owned()?;

        match &mut self.content {
            IOChunkContent::Owned {
                content,
                has_newline,
                ..
            } => Ok((content, has_newline)),
            #[allow(unreachable_patterns)]
            _ => unreachable!("ensure_owned should convert to Owned"),
        }
    }
}

/// Data to be written to a file. It can come from the mmapped
/// memory space, in which case it is tracked to allow coallescing
/// and bypassing BufWriter, or it can be other data from the process's
/// memory space.
#[derive(Debug, PartialEq, Eq)]
enum IOChunkContent<'a> {
    #[cfg(unix)]
    MmapInput {
        content: &'a [u8],   // Line without newline
        full_span: &'a [u8], // Line including original newline, if any
    },
    Owned {
        content: String,   // Line content without newline
        has_newline: bool, // True if \n-terminated
        #[cfg(not(unix))]
        _phantom: PhantomData<&'a ()>, // Silence E0392 warning
    },
}

impl IOChunkContent<'_> {
    /// Construct a new Owned chunk.
    pub fn new_owned(content: String, has_newline: bool) -> Self {
        #[cfg(unix)]
        return IOChunkContent::Owned {
            content,
            has_newline,
        };

        #[cfg(not(unix))]
        return IOChunkContent::Owned {
            content,
            has_newline,
            // Avoid E0063 missing _phantom initialization errors
            _phantom: std::marker::PhantomData,
        };
    }

    #[cfg(unix)]
    unsafe fn as_str_unchecked(&self) -> &str {
        match self {
            IOChunkContent::MmapInput { content, .. } => unsafe {
                std::str::from_utf8_unchecked(content)
            },
            IOChunkContent::Owned { content, .. } => content,
        }
    }

    /// Return the content's length (in bytes or characters).
    pub fn len(&self) -> usize {
        match self {
            #[cfg(unix)]
            IOChunkContent::MmapInput { content, .. } => content.len(),

            IOChunkContent::Owned { content, .. } => content.len(),
        }
    }
}

/// Unified reader that uses mmap when possible, falls back to buffered reading.
pub enum LineReader {
    #[cfg(unix)]
    MmapInput {
        mapped_file: Mmap, // A handle that can derive the mapped file slice
        cursor: MmapLineCursor<'static>,
    },
    ReadInput(ReadLineCursor),
}

/// Return a LineReader that uses the ReadInput method fot the specified file.
fn line_reader_read_input(file: File) -> io::Result<LineReader> {
    let boxed: Box<dyn Read> = Box::new(file);
    let reader = BufReader::new(boxed);
    Ok(LineReader::ReadInput(ReadLineCursor::new(reader)))
}

impl LineReader {
    /// Open the specified file for line input.
    // Use "-" to read from the standard input.
    pub fn open(path: &PathBuf) -> io::Result<Self> {
        if path.as_os_str() == "-" {
            let stdin = io::stdin();
            let boxed: Box<dyn Read> = Box::new(stdin.lock());
            let reader = BufReader::new(boxed);
            return Ok(LineReader::ReadInput(ReadLineCursor::new(reader)));
        }

        let file = File::open(path)?;

        #[cfg(unix)]
        {
            match unsafe { Mmap::map(&file) } {
                Ok(mapped_file) => {
                    // SAFETY: mmap owns the data and lives in the same variant
                    let slice: &'static [u8] = unsafe {
                        std::slice::from_raw_parts(mapped_file.as_ptr(), mapped_file.len())
                    };
                    let cursor = MmapLineCursor::new(slice);
                    Ok(LineReader::MmapInput {
                        mapped_file,
                        cursor,
                    })
                }
                // Fallback to ReadInput
                Err(_) => line_reader_read_input(file),
            }
        }

        #[cfg(not(unix))]
        {
            line_reader_read_input(file)
        }
    }

    /// Open the specified file to read as a stream.
    #[cfg(test)]
    pub fn open_stream(path: &PathBuf) -> io::Result<Self> {
        let file = File::open(path)?;
        line_reader_read_input(file)
    }

    /// Return the next line, if available and also the availability
    /// of another one, or None at end of file.
    pub fn get_line(&mut self) -> io::Result<Option<(IOChunk, bool)>> {
        match self {
            #[cfg(unix)]
            LineReader::MmapInput { cursor, .. } => {
                if let Some(NextMmapLine {
                    content,
                    full_span,
                    is_last_line,
                }) = cursor.get_line()?
                {
                    let chunk =
                        IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });

                    Ok(Some((chunk, is_last_line)))
                } else {
                    Ok(None)
                }
            }

            LineReader::ReadInput(cursor) => {
                if let Some((line, _has_newline, is_last_line)) = cursor.get_line()? {
                    let chunk =
                        IOChunk::from_content(IOChunkContent::new_owned(line, _has_newline));
                    Ok(Some((chunk, is_last_line)))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

// Define a trait combining two: workaround for Rust's corresponding inability.
#[cfg(unix)]
pub trait OutputWrite: Write + AsRawFd {}
#[cfg(unix)]
impl<T: Write + AsRawFd> OutputWrite for T {}

#[cfg(not(unix))]
pub trait OutputWrite: Write {}
#[cfg(not(unix))]
impl<T: Write> OutputWrite for T {}

/// Abstraction for outputting data, potentially from the mmapped file
/// Outputs from mmapped data are coallesced and written via a write(2)
/// system call without any copying if worthwhile.
/// All other output is buffered and writen via BufWriter.
pub struct OutputBuffer {
    out: BufWriter<Box<dyn OutputWrite + 'static>>, // Where to write
    #[cfg(unix)]
    mmap_ptr: Option<(*const u8, usize)>, // Start and len of chunk to write
    #[cfg(test)]
    writes_issued: usize,           // Number of issued write(2) calls
}

/// Wrapper that issues the write(2) system call
#[cfg(unix)]
fn write_syscall(fd: i32, ptr: *const u8, len: usize) -> io::Result<()> {
    let ret = unsafe { write(fd, ptr as *const c_void, len) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Threshold to use buffered writes for output
// These 4k are half the 8k size of the BufWriter buffer.
// The constant guarantees that, at worst, mmapped output will
// result in a doubling of the issued write(2) system calls.
// Taking into account the non-copied data, this should result
// in overall fewer CPU instructions.
#[cfg(unix)]
const MIN_DIRECT_WRITE: usize = 4 * 1024;

/// The maximum size of a pending write buffer
// Once more than 64k accumulate, issue a write to allow the OS
// and downstream pipes to handle the output processing in parallel
// with our processing.
#[cfg(unix)]
const MAX_PENDING_WRITE: usize = 64 * 1024;

impl OutputBuffer {
    pub fn new(w: Box<dyn OutputWrite + 'static>) -> Self {
        Self {
            out: BufWriter::new(w),
            #[cfg(unix)]
            mmap_ptr: None,
            #[cfg(test)]
            writes_issued: 0,
        }
    }

    /// Schedule the specified String or &strfor eventual output
    pub fn write_str<S: Into<String>>(&mut self, s: S) -> io::Result<()> {
        self.write_chunk(&IOChunk::from_content(IOChunkContent::new_owned(
            s.into(),
            false,
        )))
    }

    /// Copy the specified file to the output.
    pub fn copy_file(&mut self, path: &PathBuf) -> io::Result<()> {
        #[cfg(unix)]
        self.flush_mmap()?; // Flush mmap writes, if any.

        let file = match File::open(path) {
            Ok(f) => f,
            // Per POSIX, if the file can't be read treat it as empty.
            Err(_) => return Ok(()),
        };

        let mut reader = BufReader::new(file);
        io::copy(&mut reader, &mut self.out)?;
        Ok(())
    }
}

/// Implementation of the std::io::Write trait
impl Write for OutputBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s =
            std::str::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.write_str(s)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush()
    }
}

#[cfg(unix)]
impl OutputBuffer {
    /// Schedule the specified output chunk for eventual output
    pub fn write_chunk(&mut self, chunk: &IOChunk) -> io::Result<()> {
        match &chunk.content {
            IOChunkContent::MmapInput { full_span, .. } => {
                let ptr = full_span.as_ptr();
                let len = full_span.len();

                if let Some((p, l)) = self.mmap_ptr {
                    // Coalesce if adjacent
                    if unsafe { p.add(l) } == ptr && l < MAX_PENDING_WRITE {
                        self.mmap_ptr = Some((p, l + len));
                        return Ok(());
                    } else {
                        self.flush_mmap()?; // not contiguous
                    }
                }
                self.mmap_ptr = Some((ptr, len));
                Ok(())
            }

            IOChunkContent::Owned {
                content,
                has_newline,
                ..
            } => {
                self.flush_mmap()?;
                self.out.write_all(content.as_bytes())?;
                if *has_newline {
                    self.out.write_all(b"\n")?;
                }
                Ok(())
            }
        }
    }

    // Flush any pending mmap data
    #[cfg(unix)]
    fn flush_mmap(&mut self) -> io::Result<()> {
        if let Some((ptr, len)) = self.mmap_ptr.take() {
            if len < MIN_DIRECT_WRITE {
                // SAFELY treat as &[u8] and write to buffered writer
                let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
                return self.out.write_all(slice);
            } else {
                // Large enough: write directly using zero-copy
                let fd = self.out.get_ref().as_raw_fd();
                self.out.flush()?; // sync any buffered data
                #[cfg(test)]
                {
                    self.writes_issued += 1;
                }
                return write_syscall(fd, ptr, len);
            }
        }
        Ok(())
    }

    /// Flush everything: pending mmap and buffered data.
    pub fn flush(&mut self) -> io::Result<()> {
        self.flush_mmap()?; // flush mmap if any
        self.out.flush() // then flush buffered data
    }
}

#[cfg(not(unix))]
impl OutputBuffer {
    /// Schedule the specified output chunk for eventual output
    pub fn write_chunk(&mut self, chunk: &IOChunk) -> io::Result<()> {
        match &chunk.content {
            IOChunkContent::Owned {
                content,
                has_newline,
                ..
            } => {
                self.out.write_all(content.as_bytes())?;
                if *has_newline {
                    self.out.write_all(b"\n")?;
                }
                Ok(())
            }
        }
    }

    /// Flush everything: pending mmap and buffered data.
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush() // then flush buffered data
    }
}

// Usage example (never compiled)
#[cfg(any())]
pub fn main() -> io::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "-".into());
    let mut reader = LineReader::open(&path)?;
    let stdout = Box::new(io::stdout().lock());
    let mut output = OutputBuffer::new(stdout);

    while let Some(chunk) = reader.get_line()? {
        output.write_chunk(&chunk)?;
    }

    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::fs::File;
    #[cfg(unix)]
    use std::io::{self, Write};
    use tempfile::NamedTempFile;

    /// Helper: produce a 4k-byte Vec of `'.'`s ending in `'\n'`.
    #[cfg(unix)]
    fn make_dot_line_4k() -> Vec<u8> {
        let mut buf = Vec::with_capacity(4096);
        buf.extend(std::iter::repeat(b'.').take(4095));
        buf.push(b'\n');
        buf
    }

    #[test]
    fn test_owned_line_output() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        {
            let file = tmp.reopen()?;
            let mut out = OutputBuffer::new(Box::new(file));
            out.write_str("foo\n")?;
            out.write_str("bar\n")?;
            out.flush()?;
            assert_eq!(out.writes_issued, 0);
        } // File closes here as it leaves the scope

        let contents = fs::read(tmp.path())?;
        assert_eq!(contents.as_slice(), b"foo\nbar\n");
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_mmap_line_output_single() -> io::Result<()> {
        use std::fs;
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Prepare the input buffer: two lines in one contiguous mmap region
        let mmap_data = b"line one\nline two\n";

        // Write that into a temp file
        let mut input = NamedTempFile::new()?;
        input.write_all(mmap_data)?;
        input.flush()?;
        let input_path = input.path().to_path_buf();

        // Open the reader on that file
        let mut reader = LineReader::open(&input_path)?;

        // Prepare an output temp file and wrap it in our OutputBuffer
        let output = NamedTempFile::new()?;
        let output_path = output.path().to_path_buf();
        let out_file = std::fs::File::create(&output_path)?;
        let mut out = OutputBuffer::new(Box::new(Box::new(out_file)));

        // Drain reader → writer
        while let Some((chunk, _last_line)) = reader.get_line()? {
            out.write_chunk(&chunk)?;
        }
        out.flush()?;

        assert_eq!(out.writes_issued, 0);

        let written = fs::read(&output_path)?;
        assert_eq!(written.as_slice(), mmap_data);

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_mixed_output_order_preserved() -> io::Result<()> {
        use std::fs;
        use std::fs::File;
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Prepare an input file containing two lines: "zero\none\n"
        let data = b"zero\none\n";
        let mut input = NamedTempFile::new()?;
        input.write_all(data)?;
        input.flush()?;
        let input_path = input.path().to_path_buf();
        let mut reader = LineReader::open(&input_path)?;

        // Prepare an empty output file
        let output = NamedTempFile::new()?;
        let output_path = output.path().to_path_buf();
        let out_file = File::create(&output_path)?;
        let mut out = OutputBuffer::new(Box::new(out_file));

        // Read the first mmap line ("zero\n") and write it
        if let Some((chunk, _last_line)) = reader.get_line()? {
            out.write_chunk(&chunk)?;
        }

        // Write an owned line ("middle\n")
        out.write_str("middle\n")?;

        // Read the second mmap line ("one\n") and write it
        if let Some((chunk, _last_line)) = reader.get_line()? {
            out.write_chunk(&chunk)?;
        }

        out.flush()?;

        // Since all writes are small (<4K), we expect zero zero copy syscalls
        assert_eq!(out.writes_issued, 0);

        // Read both files back and compare
        let expected = {
            let mut v = Vec::new();
            v.extend_from_slice(b"zero\n");
            v.extend_from_slice(b"middle\n");
            v.extend_from_slice(b"one\n");
            v
        };
        let actual = fs::read(&output_path)?;
        assert_eq!(actual, expected);

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_large_file_zero_copy() -> io::Result<()> {
        // Create and fill the input temp file:
        let mut input = NamedTempFile::new()?;
        write!(input, "first line\nsecond line\n")?;
        let dot_line = make_dot_line_4k();
        input.write_all(&dot_line)?;
        input.flush()?;
        let input_path = input.path().to_path_buf();

        // Open reader on input file:
        let mut reader = LineReader::open(&input_path)?;

        // Create the output temp file (empty):
        let output = NamedTempFile::new()?;
        let output_path = output.path().to_path_buf();
        let out_file = File::create(&output_path)?;

        // Wrap it in your OutputBuffer and run the loop:
        let mut out = OutputBuffer::new(Box::new(out_file));
        let mut nline = 0;
        while let Some((chunk, _last_line)) = reader.get_line()? {
            out.write_chunk(&chunk)?;
            nline += 1;
        }
        assert_eq!(nline, 3);

        out.flush()?;
        assert_eq!(out.writes_issued, 1);

        // Verify that files match:
        let expected = fs::read(&input_path)?;
        let actual = fs::read(&output_path)?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_large_file_zero_copy_unterminated() -> io::Result<()> {
        // Create and fill the input temp file:
        let mut input = NamedTempFile::new()?;
        write!(input, "first line\nsecond line\n")?;
        let dot_line = make_dot_line_4k();
        input.write_all(&dot_line)?;
        write!(input, "last line (unterminated)")?;
        input.flush()?;
        let input_path = input.path().to_path_buf();

        // Open reader on input file:
        let mut reader = LineReader::open(&input_path)?;

        // Create the output temp file (empty):
        let output = NamedTempFile::new()?;
        let output_path = output.path().to_path_buf();
        let out_file = File::create(&output_path)?;

        // Wrap it in your OutputBuffer and run the loop:
        let mut out = OutputBuffer::new(Box::new(out_file));
        let mut nline = 0;
        while let Some((chunk, _last_line)) = reader.get_line()? {
            out.write_chunk(&chunk)?;
            nline += 1;
        }
        assert_eq!(nline, 4);

        out.flush()?;
        assert_eq!(out.writes_issued, 1);

        // Verify that files match:
        let expected = fs::read(&input_path)?;
        let actual = fs::read(&output_path)?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn test_small_file_unterminated() -> io::Result<()> {
        // Create and fill the input temp file:
        let mut input = NamedTempFile::new()?;
        write!(input, "first line\nsecond line\nlast line (unterminated)")?;
        input.flush()?;
        let input_path = input.path().to_path_buf();

        // Open reader on input file:
        let mut reader = LineReader::open(&input_path)?;

        // Create the output temp file (empty):
        let output = NamedTempFile::new()?;
        let output_path = output.path().to_path_buf();
        let out_file = File::create(&output_path)?;

        // Wrap it in your OutputBuffer and run the loop:
        let mut out = OutputBuffer::new(Box::new(out_file));
        let mut nline = 0;
        while let Some((chunk, _last_line)) = reader.get_line()? {
            out.write_chunk(&chunk)?;
            nline += 1;
        }
        assert_eq!(nline, 3);

        out.flush()?;
        assert_eq!(out.writes_issued, 0);

        // Verify that files match:
        let expected = fs::read(&input_path)?;
        let actual = fs::read(&output_path)?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn test_small_file_unterminated_stream() -> io::Result<()> {
        // Create and fill the input temp file:
        let mut input = NamedTempFile::new()?;
        write!(input, "first line\nsecond line\nlast line (unterminated)")?;
        input.flush()?;
        let input_path = input.path().to_path_buf();

        // Open reader on input file:
        let mut reader = LineReader::open_stream(&input_path)?;

        // Create the output temp file (empty):
        let output = NamedTempFile::new()?;
        let output_path = output.path().to_path_buf();
        let out_file = File::create(&output_path)?;

        // Wrap it in your OutputBuffer and run the loop:
        let mut out = OutputBuffer::new(Box::new(out_file));
        let mut nline = 0;
        while let Some((chunk, _last_line)) = reader.get_line()? {
            out.write_chunk(&chunk)?;
            nline += 1;
        }
        assert_eq!(nline, 3);

        out.flush()?;
        assert_eq!(out.writes_issued, 0);

        // Verify that files match:
        let expected = fs::read(&input_path)?;
        let actual = fs::read(&output_path)?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_large_file_zero_copy_with_flush() -> io::Result<()> {
        // Create and fill the input temp file:
        let mut input = NamedTempFile::new()?;
        write!(input, "first line\nsecond line\n")?;
        let dot_line = make_dot_line_4k();
        // Write 64k + 16k to ensure one flush when writing
        for _i in 0..20 {
            input.write_all(&dot_line)?;
        }
        input.flush()?;
        let input_path = input.path().to_path_buf();

        // Open reader on input file:
        let mut reader = LineReader::open(&input_path)?;

        // Create the output temp file (empty):
        let output = NamedTempFile::new()?;
        let output_path = output.path().to_path_buf();
        let out_file = File::create(&output_path)?;

        // Wrap it in your OutputBuffer and run the loop:
        let mut out = OutputBuffer::new(Box::new(out_file));
        let mut nline = 0;
        while let Some((chunk, _last_line)) = reader.get_line()? {
            out.write_chunk(&chunk)?;
            nline += 1;
        }
        assert_eq!(nline, 22);

        out.flush()?;
        assert_eq!(out.writes_issued, 2);

        // Verify that files match:
        let expected = fs::read(&input_path)?;
        let actual = fs::read(&output_path)?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn test_stream_read() -> std::io::Result<()> {
        // Create temporary file with known contents
        let mut tmp = NamedTempFile::new()?;
        write!(tmp, "first line\nsecond line\nlast line\n")?;
        tmp.flush()?;

        let path = tmp.path().to_path_buf();
        let mut reader = LineReader::open_stream(&path)?;

        // Verify the reader's operation
        if let Some((
            IOChunk {
                content:
                    IOChunkContent::Owned {
                        content,
                        has_newline,
                        ..
                    },
                utf8_verified,
                ..
            },
            last_line,
        )) = reader.get_line()?
        {
            assert_eq!(content, "first line");
            assert_eq!(content.len(), 10);
            assert!(has_newline);
            assert!(!utf8_verified.get());
            assert!(!last_line);
        } else {
            panic!("Expected IOChunkContent::Owned");
        }

        if let Some((
            IOChunk {
                content:
                    IOChunkContent::Owned {
                        content,
                        has_newline,
                        ..
                    },
                ..
            },
            last_line,
        )) = reader.get_line()?
        {
            assert_eq!(content, "second line");
            assert!(has_newline);
            assert!(!last_line);
        } else {
            panic!("Expected IOChunkContent::Owned");
        }

        if let Some((content, last_line)) = reader.get_line()? {
            assert_eq!(content.as_str().unwrap(), "last line");
            assert!(last_line);
        } else {
            panic!("Expected IOChunk");
        }

        assert_eq!(reader.get_line()?, None);

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_mmap_read() -> std::io::Result<()> {
        // Create temporary file with known contents
        let mut tmp = NamedTempFile::new()?;
        write!(tmp, "first line\nsecond line\nlast line\n")?;
        tmp.flush()?;

        let path = tmp.path().to_path_buf();
        let mut reader = LineReader::open(&path)?;

        // Verify the reader's operation
        if let Some((
            IOChunk {
                content:
                    IOChunkContent::MmapInput {
                        content, full_span, ..
                    },
                utf8_verified,
                ..
            },
            last_line,
        )) = reader.get_line()?
        {
            assert_eq!(content, b"first line");
            assert_eq!(content.len(), 10);
            assert_eq!(full_span, b"first line\n");
            assert!(!utf8_verified.get());
            assert!(!last_line);
        } else {
            panic!("Expected IOChunkContent::MapInput");
        }

        if let Some((
            IOChunk {
                content:
                    IOChunkContent::MmapInput {
                        content, full_span, ..
                    },
                utf8_verified,
                ..
            },
            last_line,
        )) = reader.get_line()?
        {
            assert_eq!(content, b"second line");
            assert_eq!(full_span, b"second line\n");
            assert!(!utf8_verified.get());
            assert!(!last_line);
        } else {
            panic!("Expected IOChunkContent::MapInput");
        }

        if let Some((content, last_line)) = reader.get_line()? {
            assert_eq!(content.as_bytes(), b"last line");
            assert_eq!(content.as_str().unwrap(), "last line");
            assert!(content.utf8_verified.get());
            assert!(last_line);
            // Cached version
            assert_eq!(content.as_str().unwrap(), "last line");
        } else {
            panic!("Expected IOChunk");
        }

        assert_eq!(reader.get_line()?, None);

        Ok(())
    }

    // is_newline_terminated, is_empty
    #[test]
    fn test_owned_newline_terminated_non_empty() {
        let chunk = IOChunk::from_content(IOChunkContent::new_owned("line".to_string(), true));
        assert!(chunk.is_newline_terminated());
        assert!(!chunk.is_empty());
    }

    #[test]
    fn test_owned_newline_terminated_empty() {
        let chunk = IOChunk::from_content(IOChunkContent::new_owned("".to_string(), true));
        assert!(chunk.is_newline_terminated());
        assert!(chunk.is_empty());
    }

    #[test]
    fn test_owned_not_newline_terminated() {
        let chunk = IOChunk::from_content(IOChunkContent::new_owned("line".to_string(), false));
        assert!(!chunk.is_newline_terminated());
    }

    #[cfg(unix)]
    #[test]
    fn test_mmap_newline_terminated() {
        let content = b"line";
        let full_span = b"line\n";
        let chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });
        assert!(chunk.is_newline_terminated());
    }

    #[cfg(unix)]
    #[test]
    fn test_mmap_not_newline_terminated() {
        let content = b"line";
        let full_span = b"line";
        let chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });
        assert!(!chunk.is_newline_terminated());
    }

    #[cfg(unix)]
    #[test]
    fn test_mmap_empty() {
        let content = b"";
        let full_span = b"";
        let chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });
        assert!(!chunk.is_newline_terminated());
    }

    // ensure_owned()
    #[test]
    fn test_ensure_owned_on_owned() {
        let mut chunk =
            IOChunk::from_content(IOChunkContent::new_owned("already owned".to_string(), true));

        let result = chunk.ensure_owned();
        assert!(result.is_ok());

        // Content must be unchanged
        match &chunk.content {
            IOChunkContent::Owned {
                content,
                has_newline,
                ..
            } => {
                assert_eq!(content, "already owned");
                assert!(*has_newline);
            }
            #[cfg(unix)]
            _ => panic!("Expected Owned variant"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_owned_on_mmap_valid_utf8() {
        let content = b"mmap string";
        let full_span = b"mmap string\n";

        let mut chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });

        let result = chunk.ensure_owned();
        assert!(result.is_ok());

        match &chunk.content {
            IOChunkContent::Owned {
                content,
                has_newline,
                ..
            } => {
                assert_eq!(content, "mmap string");
                assert!(*has_newline);
            }
            _ => panic!("Expected Owned variant after ensure_owned"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_owned_on_mmap_valid_utf8_no_newline() {
        let content = b"no newline";
        let full_span = b"no newline";

        let mut chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });

        let result = chunk.ensure_owned();
        assert!(result.is_ok());

        match &chunk.content {
            IOChunkContent::Owned {
                content,
                has_newline,
                ..
            } => {
                assert_eq!(content, "no newline");
                assert!(!*has_newline);
            }
            _ => panic!("Expected Owned variant after ensure_owned"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_owned_on_mmap_invalid_utf8() {
        let content = b"bad\xFFutf8";
        let full_span = b"bad\xFFutf8\n";

        let mut chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });

        let result = chunk.ensure_owned();
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("invalid utf-8"),
            "Unexpected error message: {}",
            err_msg
        );
    }

    // fields_mut
    #[test]
    fn test_fields_mut_on_owned() {
        let mut chunk =
            IOChunk::from_content(IOChunkContent::new_owned("hello".to_string(), false));

        let (s, _) = chunk.fields_mut().unwrap();
        s.push_str(" world");

        assert_eq!(chunk.as_str().unwrap(), "hello world");
    }

    #[cfg(unix)]
    #[test]
    fn test_fields_mut_on_mmap_input_valid_utf8() {
        let content = b"foo";
        let full_span = b"foo\n";
        let mut chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });

        {
            let (s, _) = chunk.fields_mut().unwrap();
            s.push_str("bar");
        }

        assert_eq!(chunk.as_str().unwrap(), "foobar");
    }

    #[cfg(unix)]
    #[test]
    fn test_fields_mut_on_utf8_multibyte() {
        let content = "Ζωντανά!".as_bytes();
        let full_span = "Ζωντανά!\n".as_bytes();
        let mut chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });

        let (s, _) = chunk.fields_mut().unwrap();
        s.push_str(" Δεδομένα");

        assert_eq!(chunk.as_str().unwrap(), "Ζωντανά! Δεδομένα");
    }

    #[cfg(unix)]
    #[test]
    fn test_fields_mut_invalid_utf8() {
        let content = b"abc\xFF"; // invalid UTF-8
        let full_span = b"abc\xFF\n";
        let mut chunk = IOChunk::from_content(IOChunkContent::MmapInput { content, full_span });

        let result = chunk.fields_mut();
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("invalid utf-8"));
    }
}
