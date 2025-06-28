//! Background queued reader.
use crate::queue::Queue;
use defer_heavy::defer;
use std::io;
use std::io::{Cursor, ErrorKind, Read};
use std::sync::{Arc, OnceLock};

/// Read pipe inner state
#[derive(Debug, Default)]
struct ReadPipeInner {
    /// The actual data queue.
    queue: Arc<Queue>,
    /// Async error
    error: OnceLock<ErrorKind>,
}
impl ReadPipeInner {
    /// Background read handler thread loop.
    fn handle<T: Read + Send>(&self, mut read: T) {
        defer! {
             // This also happens on panic!
            self.queue.kill();
        }
        let mut buffer = vec![0u8; 0x1_00_00];
        loop {
            let packet = match read.read(buffer.as_mut_slice()) {
                Ok(count) => buffer[0..count].to_vec(),
                Err(err) => {
                    _ = self.error.set(err.kind());
                    return;
                }
            };

            if packet.is_empty() {
                if let Err(err) = self.queue.push(packet) {
                    _ = self.error.set(err.kind());
                }
                return;
            }
            if let Err(err) = self.queue.push(packet) {
                _ = self.error.set(err.kind());
            }
        }
    }
}

/// fake read impl that will pop from a queue and try to return immediately.
/// Read are deferred to a background thread.
#[derive(Debug)]
pub struct ReadPipe {
    /// Eof marker
    eof: bool,
    /// Non-blocking marker, if the queue is empty then we do not block on it.
    nb: bool,
    /// The background queue part.
    pipe: Arc<ReadPipeInner>,
    /// current data cursor. We may pop more data from the queue than we can consume. Excess is pushed into this cursor which is read before popping the queue.
    cursor: Cursor<Vec<u8>>,
}

impl Drop for ReadPipe {
    fn drop(&mut self) {
        self.pipe.queue.kill();
    }
}

impl ReadPipe {
    /// Constructor that spawns the background thread.
    pub fn new<R: Read + Send + 'static, T: FnMut(Box<dyn FnOnce() + Send>) -> io::Result<()>>(
        write: R,
        spawner: &mut T,
    ) -> io::Result<Self> {
        let wp = Arc::new(ReadPipeInner::default());
        let wpc = Arc::clone(&wp);
        spawner(Box::new(move || {
            wpc.handle(write);
        }))?;
        Ok(Self {
            nb: false,
            eof: false,
            pipe: wp,
            cursor: Cursor::default(),
        })
    }

    /// is nb on?
    pub fn nb(&mut self, value: bool) {
        self.nb = value;
    }

    /// get a handle to the internal queue.
    pub fn dup_queue(&self) -> Arc<Queue> {
        Arc::clone(&self.pipe.queue)
    }

    /// util to get the error. All errors are treated as fatal.
    /// if this fn is called when there is no error it will set the error to `BrokenPipe`.
    /// and kill the background thread.
    fn fetch_err(&self) -> io::Error {
        self.pipe.queue.kill();
        if let Some(err) = self.pipe.error.get().copied() {
            return io::Error::from(err);
        }
        io::Error::from(ErrorKind::BrokenPipe)
    }
}

impl Read for ReadPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.eof || buf.is_empty() {
            return Ok(0);
        }

        loop {
            let size = self.cursor.read(buf)?;
            if size != 0 {
                return Ok(size);
            }

            if self.nb {
                match self.pipe.queue.try_pop() {
                    Ok(Some(data)) => {
                        if data.is_empty() {
                            self.eof = true;
                            return Ok(0);
                        }

                        self.cursor = Cursor::new(data);
                        continue;
                    }
                    Ok(None) => {
                        return Err(io::Error::from(ErrorKind::WouldBlock)); //Will be cought.
                    }
                    Err(err) => {
                        _ = self.pipe.error.set(err.kind());
                        return Err(self.fetch_err());
                    }
                }
            }

            match self.pipe.queue.pop() {
                Ok(data) => {
                    if data.is_empty() {
                        self.eof = true;
                        return Ok(0);
                    }

                    self.cursor = Cursor::new(data);
                    continue;
                }
                Err(err) => {
                    _ = self.pipe.error.set(err.kind());
                    return Err(self.fetch_err());
                }
            }
        }
    }
}
