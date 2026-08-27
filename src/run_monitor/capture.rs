//! Bounded command output capture.

use std::io::{self, Read, Write};
use std::thread;

use thiserror::Error;

const BUFFER_SIZE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureBudget {
    cap: usize,
    written: usize,
    discarded: usize,
}

impl CaptureBudget {
    pub(crate) const fn new(cap: usize) -> Result<Self, CaptureBudgetError> {
        if cap == 0 {
            return Err(CaptureBudgetError);
        }
        Ok(Self {
            cap,
            written: 0,
            discarded: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn consume(&mut self, bytes: usize) -> (usize, usize) {
        let written = self.remaining().min(bytes);
        let discarded = bytes.saturating_sub(written);
        self.written = self.written.saturating_add(written);
        self.discarded = self.discarded.saturating_add(discarded);
        (written, discarded)
    }

    const fn remaining(self) -> usize {
        self.cap.saturating_sub(self.written)
    }

    fn record_chunk(&mut self, written: usize, consumed: usize) {
        self.written = self.written.saturating_add(written);
        self.discarded = self
            .discarded
            .saturating_add(consumed.saturating_sub(written));
    }

    pub(crate) const fn written(self) -> usize {
        self.written
    }

    pub(crate) const fn discarded(self) -> usize {
        self.discarded
    }

    #[cfg(test)]
    pub(crate) const fn truncated(self) -> bool {
        self.discarded > 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureBudgetError;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StreamCaptureSummary {
    written: usize,
    discarded: usize,
}

impl StreamCaptureSummary {
    #[cfg(test)]
    pub(crate) const fn written(self) -> usize {
        self.written
    }

    #[cfg(test)]
    pub(crate) const fn discarded(self) -> usize {
        self.discarded
    }

    pub(crate) const fn truncated(self) -> bool {
        self.discarded > 0
    }
}

pub(crate) struct CapturedStream<W> {
    _writer: W,
    summary: StreamCaptureSummary,
    error: Option<io::Error>,
}

impl<W> CapturedStream<W> {
    #[cfg(test)]
    #[allow(clippy::used_underscore_binding)]
    pub(crate) const fn writer(&self) -> &W {
        &self._writer
    }

    pub(crate) const fn summary(&self) -> StreamCaptureSummary {
        self.summary
    }

    pub(crate) const fn error(&self) -> Option<&io::Error> {
        self.error.as_ref()
    }
}

pub(crate) struct CapturedStreams<Stdout, Stderr> {
    pub(crate) stdout: CapturedStream<Stdout>,
    pub(crate) stderr: CapturedStream<Stderr>,
}

pub(crate) fn capture_streams<OutReader, ErrReader, OutWriter, ErrWriter>(
    stdout: OutReader,
    stderr: ErrReader,
    stdout_writer: OutWriter,
    stderr_writer: ErrWriter,
    cap: usize,
) -> Result<CapturedStreams<OutWriter, ErrWriter>, CaptureError>
where
    OutReader: Read + Send,
    ErrReader: Read + Send,
    OutWriter: Write + Send,
    ErrWriter: Write + Send,
{
    let stdout_budget = CaptureBudget::new(cap).map_err(|_| CaptureError::InvalidCap)?;
    let stderr_budget = CaptureBudget::new(cap).map_err(|_| CaptureError::InvalidCap)?;
    let (stdout_result, stderr_result) = thread::scope(|scope| {
        let stdout_handle = scope.spawn(move || drain_stream(stdout, stdout_writer, stdout_budget));
        let stderr_handle = scope.spawn(move || drain_stream(stderr, stderr_writer, stderr_budget));
        (stdout_handle.join(), stderr_handle.join())
    });
    let stdout = stdout_result.map_err(|_| CaptureError::ThreadPanicked)?;
    let stderr = stderr_result.map_err(|_| CaptureError::ThreadPanicked)?;
    Ok(CapturedStreams { stdout, stderr })
}

fn drain_stream<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    mut budget: CaptureBudget,
) -> CapturedStream<W> {
    let mut buffer = [0_u8; BUFFER_SIZE];
    let mut first_error = None;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                first_error = Some(error);
                break;
            }
        };
        let allowed = if first_error.is_none() {
            budget.remaining().min(read)
        } else {
            0
        };
        let mut written = 0;
        while written < allowed {
            match writer.write(&buffer[written..allowed]) {
                Ok(0) => {
                    first_error = Some(io::Error::from(io::ErrorKind::WriteZero));
                    break;
                }
                Ok(count) => written += count,
                Err(error) => {
                    first_error = Some(error);
                    break;
                }
            }
        }
        budget.record_chunk(written, read);
    }
    if let Err(error) = writer.flush() {
        if first_error.is_none() {
            first_error = Some(error);
        }
    }
    CapturedStream {
        _writer: writer,
        summary: StreamCaptureSummary {
            written: budget.written(),
            discarded: budget.discarded(),
        },
        error: first_error,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum CaptureError {
    #[error("capture cap must be greater than zero")]
    InvalidCap,
    #[error("capture worker panicked")]
    ThreadPanicked,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::io::{self, Cursor, Write};

    use super::{CaptureBudget, capture_streams};
    use crate::application::ElapsedClock;
    use crate::domain::{Environment, ExecutionMode, ExecutionSpec};
    use crate::infrastructure::process::{NativeProcessInspector, NativeProcessRunner};
    use crate::infrastructure::time::NativeClock;

    #[test]
    fn cap_boundary_tracks_written_and_discarded_bytes() {
        let mut budget = CaptureBudget::new(5).expect("valid cap");
        assert_eq!(budget.consume(3), (3, 0));
        assert_eq!(budget.consume(2), (2, 0));
        assert_eq!(budget.consume(1), (0, 1));
        assert_eq!(budget.written(), 5);
        assert_eq!(budget.discarded(), 1);
        assert!(budget.truncated());
        assert!(CaptureBudget::new(0).is_err());
    }

    #[test]
    fn large_streams_are_drained_concurrently_and_capped() {
        let output = capture_streams(
            Cursor::new(vec![b'a'; 256 * 1024]),
            Cursor::new(vec![b'b'; 256 * 1024]),
            Vec::new(),
            Vec::new(),
            1_024,
        )
        .expect("capture threads");
        assert_eq!(output.stdout.summary().written(), 1_024);
        assert_eq!(output.stderr.summary().written(), 1_024);
        assert!(output.stdout.summary().truncated());
        assert!(output.stderr.summary().truncated());
        assert_eq!(output.stdout.writer().len(), 1_024);
        assert_eq!(output.stderr.writer().len(), 1_024);
    }

    #[test]
    fn empty_closed_streams_finish_without_error() {
        let output = capture_streams(
            Cursor::new(Vec::<u8>::new()),
            Cursor::new(Vec::<u8>::new()),
            Vec::new(),
            Vec::new(),
            16,
        )
        .expect("capture threads");
        assert_eq!(output.stdout.summary().written(), 0);
        assert_eq!(output.stderr.summary().discarded(), 0);
        assert!(output.stdout.error().is_none());
        assert!(output.stderr.error().is_none());
    }

    #[test]
    fn writer_failure_does_not_stop_pipe_drain() {
        let input = vec![b'x'; 64 * 1024];
        let output = capture_streams(
            Cursor::new(input.clone()),
            Cursor::new(Vec::<u8>::new()),
            FailingWriter::new(7),
            Vec::new(),
            input.len(),
        )
        .expect("capture threads");
        assert!(output.stdout.error().is_some());
        assert!(output.stdout.summary().discarded() > 0);
        assert_eq!(
            output.stdout.summary().written() + output.stdout.summary().discarded(),
            input.len()
        );
    }

    #[test]
    fn concurrent_child_streams_do_not_deadlock() {
        let clock = NativeClock;
        let runner = NativeProcessRunner::new(NativeProcessInspector::new(
            clock.boot_identity().expect("boot identity"),
        ));
        let execution = ExecutionSpec::new(
            ExecutionMode::Shell,
            vec![
                "i=0; while [ \"$i\" -lt 20000 ]; do printf x; printf y >&2; i=$((i+1)); done"
                    .to_owned(),
            ],
            "/".to_owned(),
            Environment::from_pairs([("PATH", "/usr/bin:/bin")]).expect("environment"),
        )
        .expect("execution");
        let mut child = runner.spawn(&execution).expect("spawn");
        let stdout = child.child_mut().stdout.take().expect("captured stdout");
        let stderr = child.child_mut().stderr.take().expect("captured stderr");
        let output =
            capture_streams(stdout, stderr, Vec::new(), Vec::new(), 1_024).expect("capture");
        assert!(child.child_mut().wait().expect("wait").success());
        assert_eq!(output.stdout.summary().written(), 1_024);
        assert_eq!(output.stderr.summary().written(), 1_024);
        assert!(output.stdout.summary().truncated());
        assert!(output.stderr.summary().truncated());
    }

    struct FailingWriter {
        remaining: usize,
    }

    impl FailingWriter {
        const fn new(remaining: usize) -> Self {
            Self { remaining }
        }
    }

    impl Write for FailingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "full"));
            }
            let written = self.remaining.min(buffer.len());
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingFlushWriter;

    impl Write for FailingFlushWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "flush refused"))
        }
    }

    #[test]
    fn flush_failure_is_surfaced_as_a_stream_error() {
        let output = capture_streams(
            Cursor::new(Vec::<u8>::new()),
            Cursor::new(b"ignored".to_vec()),
            FailingFlushWriter,
            Vec::new(),
            16,
        )
        .expect("capture threads");
        assert!(output.stdout.error().is_some());
        assert!(output.stderr.error().is_none());
        // Every drained byte was accepted by the writer; only the flush lied.
        assert_eq!(output.stdout.summary().written(), 0);
        assert!(!output.stdout.summary().truncated());
    }
}
