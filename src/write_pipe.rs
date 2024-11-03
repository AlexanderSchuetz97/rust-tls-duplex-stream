//! Background queued writer.
use crate::queue::Queue;
use defer_heavy::defer;
use std::io;
use std::io::{ErrorKind, Write};
use std::sync::{Arc, OnceLock};

/// Write pipe inner state
#[derive(Debug, Default)]
struct WritePipeInner {
    /// The actual data queue.
    queue: Arc<Queue>,
    /// Async error
    error: OnceLock<ErrorKind>,
}

impl WritePipeInner {

    /// Background write handler thread loop.
    fn handle<T: Write + Send>(&self, mut write: T) {
        defer! {
            // This also happens on panic!
            self.queue.kill();
        }
        loop {
            let pop = match self.queue.pop() {
                Ok(guard) => guard,
                Err(e) => {
                    _ = self.error.set(e.kind());
                    return;
                }
            };

            if let Err(err) = write.write_all(pop.as_slice()) {
                _ = self.error.set(err.kind());
                return;
            }
        }
    }
}


/// fake write impl that will push to a queue and try to return immediately. 
/// Writes are deferred to a background thread.
#[derive(Debug)]
pub struct WritePipe(Arc<WritePipeInner>);

impl Drop for WritePipe {
    fn drop(&mut self) {
        self.0.queue.kill();
    }
}

impl WritePipe {
    /// Constructor that starts the background thread.
    pub fn new<W: Write + Send + 'static, T: FnMut(Box<dyn FnOnce() + Send>) -> io::Result<()>>(
        write: W,
        spawner: &mut T,
    ) -> io::Result<Self> {
        let wp = Arc::new(WritePipeInner::default());
        let wpc = Arc::clone(&wp);
        spawner(Box::new(move || {
            wpc.handle(write);
        }))?;
        Ok(Self(wp))
    }

    /// get a handle to the internal queue.
    pub fn dup_queue(&self) -> Arc<Queue> {
        Arc::clone(&self.0.queue)
    }

    /// util to get the error. All errors are treated as fatal.
    /// if this fn is called when there is no error it will set the error to `BrokenPipe`.
    /// and kill the background thread.
    fn fetch_err(&self) -> io::Error {
        self.0.queue.kill();
        if let Some(err) = self.0.error.get().copied() {
            return io::Error::from(err);
        }
        io::Error::from(ErrorKind::BrokenPipe)
    }
}

impl Write for WritePipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.queue.push(buf.to_vec()) {
            Ok(()) => Ok(buf.len()),
            Err(err) => {
                _ = self.0.error.set(err.kind());
                Err(self.fetch_err())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        //Not implemented on purpose as this would just stall reads.
        Ok(())
    }
}
