//! Full duplex stream wrapper around rust-tls

#![deny(clippy::correctness)]
#![warn(
    clippy::perf,
    clippy::complexity,
    clippy::style,
    clippy::nursery,
    clippy::pedantic,
    clippy::clone_on_ref_ptr,
    clippy::decimal_literal_representation,
    clippy::float_cmp_const,
    clippy::missing_docs_in_private_items,
    clippy::multiple_inherent_impl,
    clippy::unwrap_used,
    clippy::cargo_common_metadata,
    clippy::used_underscore_binding
)]

mod queue;
mod read_pipe;
mod write_pipe;
use crate::queue::Queue;
use crate::read_pipe::ReadPipe;
use crate::write_pipe::WritePipe;
use rustls::{ConnectionCommon, StreamOwned};
use std::fmt::{Arguments, Debug};
use std::io::{ErrorKind, Read, Write};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, LockResult, Mutex};
use std::time::Duration;
use std::{io, thread};

#[derive(Debug)]
pub struct RustTlsDuplexStream<C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>> + Send,
{
    /// Flag for non blocking read.
    non_blocking_read: AtomicBool,
    /// Read timeout
    read_timeout: Mutex<Option<Duration>>,
    /// Write timeout
    write_timeout: Mutex<Option<Duration>>,
    /// Inner rust-tls pseudo connection
    connection: Mutex<StreamOwned<C, CombinedPipe>>,
    /// Read queue connected to the thread that reads data from the actual connection
    read_q: Arc<Queue>,
    /// Write queue connected to the thread that writes data to the actual connection
    write_q: Arc<Queue>,
    /// Guard mutex that prevents concurrent writes.
    write_mutex: Mutex<()>,
    /// Guard mutex that prevents concurrent reads.
    read_mutex: Mutex<()>,
}

impl<C, S> RustTlsDuplexStream<C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>> + Send,
    S: rustls::SideData,
{
    ///
    /// Creates a new 'unpooled' Tls stream wrapper.
    /// This is a good choice for an application such as a client
    /// that does not create connections and doesn't have a thread pool.
    ///
    /// This fn will spawn 2 new threads using `thread::Builder::new().spawn(...)`.
    /// The threads will terminate when the returned stream is dropped and the read/write errors out.
    ///
    /// # Resource Leaks
    /// Be aware that a Read which "blocks" forever will cause the thread that's reading to stay alive forever.
    /// Either set a reasonable connection read timeout so that your Read will eventually return
    /// or call your connections shutdown fn like `TcpStream::shutdown` if such a method exists
    /// to ensure that all threads are stopped and no resources are leaked.
    ///
    /// # Errors
    /// if `thread::Builder::new().spawn` fails to spawn 2 threads.
    ///
    pub fn new_unpooled<R, W>(con: C, read: R, write: W) -> io::Result<Self>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        Self::new(con, read, write, |task| {
            thread::Builder::new().spawn(task).map(|_| {})
        })
    }

    ///
    /// Creates a new Tls stream wrapper.
    ///
    /// This fn will spawn 2 new threads using  the provided thread spawner function.
    /// The thread spawner function is called exactly twice. If the first call yields an error then
    /// it's not called again. The functions ("threads") will end when the returned stream wrapper is dropped.
    ///
    /// # Resource Leaks
    /// Be aware that a Read which "blocks" forever will cause the thread that's reading to stay alive forever.
    /// Either set a reasonable connection read timeout so that your Read will eventually return
    /// or call your connections shutdown fn like `TcpStream::shutdown` if such a method exists
    /// to ensure that all threads are stopped and no resources are leaked.
    ///
    /// # Errors
    /// propagated from the spawner fn.
    ///
    pub fn new<R, W, T>(con: C, read: R, write: W, spawner: T) -> io::Result<Self>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        T: FnMut(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    {
        let pipe = CombinedPipe::new(read, write, spawner)?;
        let read_q = pipe.0.dup_queue();
        let write_q = pipe.1.dup_queue();

        Ok(Self {
            non_blocking_read: AtomicBool::new(false),
            read_q,
            write_q,
            write_mutex: Mutex::new(()),
            read_mutex: Mutex::new(()),
            connection: Mutex::new(StreamOwned::new(con, pipe)),
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
        })
    }

    /// see `Write::write`
    /// # Errors
    /// propagated from `Write::write` once subsequent writes/flushes turn into `BrokenPipe`
    pub fn write(&self, buffer: &[u8]) -> io::Result<usize> {
        let _outer_guard = unwrap_poison(self.write_mutex.lock())?; //make writes block other writes
        let timeout_copy = unwrap_poison(self.write_timeout.lock())?
            .deref()
            .as_ref()
            .copied();
        self.write_q.flush_low(timeout_copy)?;
        unwrap_poison(self.connection.lock())?.write(buffer)
    }

    /// see `Write::flush`
    /// # Errors
    /// propagated from `Write::flush` once subsequent writes/flushes turn into `BrokenPipe`
    pub fn flush(&self) -> io::Result<()> {
        let _outer_guard = unwrap_poison(self.write_mutex.lock())?; //make writes block other writes
        unwrap_poison(self.connection.lock())?.flush()?;
        self.write_q.flush_zero()
    }

    /// see `Read::read`
    /// # Errors
    /// propagated from `Read::read` once subsequent reads turn into `BrokenPipe`
    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let _outer_guard = unwrap_poison(self.read_mutex.lock())?; //make reads block other reads
        loop {
            let mut guard = unwrap_poison(self.connection.lock())?;
            guard.sock.0.nb(true); //Return instantly if no data.
            let res = guard.read(buffer);
            guard.sock.0.nb(false); //We must clear this flag or writes may go ballistic.
            return match res {
                Ok(count) => {
                    drop(guard);
                    Ok(count)
                }
                Err(err) => {
                    if self.non_blocking_read.load(SeqCst) {
                        return Err(err);
                    }
                    if err.kind() == ErrorKind::WouldBlock {
                        //We have entered the fun zone where reads would block writes
                        let timeout_copy = unwrap_poison(self.read_timeout.lock())?
                            .deref()
                            .as_ref()
                            .copied();
                        //This drops guard as oon as a handle to read_q is acquired and will return once trying to read again is meaningful.
                        self.read_q.await_pop(guard, timeout_copy)?;
                        continue;
                    }

                    drop(guard);
                    Err(err)
                }
            };
        }
    }

    /// sets the timeout for the writing operation.
    /// This has no effect on the underlying connection and purely deals with internal writing semantics.
    /// Calls to fns that writs data will return `TimedOut` if no plain text data could be written.
    /// Cause of this is likely to be that the underlying connection does not read data fast enough.
    /// This is never caused by writing too much data.
    /// # Errors
    /// In case of poisoned mutex
    ///
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *unwrap_poison(self.read_timeout.lock())? = timeout;
        Ok(())
    }

    /// Returns the current read timeout if any
    /// # Errors
    /// In case of poisoned mutex
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(unwrap_poison(self.read_timeout.lock())?.as_ref().copied())
    }

    /// sets non-blocking mode for read.
    /// This has no effect on the underlying connection and purely deals with internal reading semantics.
    /// Calls to fns that read data will return `WouldBlock` immediately if no plain text data is available to be read.
    /// # Errors
    /// In case of poisoned mutex
    pub fn set_read_non_block(&self, on: bool) -> io::Result<()> {
        self.non_blocking_read.store(on, SeqCst);
        Ok(())
    }

    /// sets the timeout for the writing operation.
    /// This has no effect on the underlying connection and purely deals with internal writing semantics.
    /// Calls to fns that writs data will return `TimedOut` if no plain text data could be written.
    /// Cause of this is likely to be that the underlying connection does not send data fast enough.
    /// This is never caused by reading too much data.
    /// # Errors
    /// In case of poisoned mutex
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *unwrap_poison(self.write_timeout.lock())? = timeout;
        Ok(())
    }

    /// Returns the current write timeout if any
    /// # Errors
    /// In case of poisoned mutex
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(unwrap_poison(self.write_timeout.lock())?.as_ref().copied())
    }

    /// See `Read::read_to_end`
    /// # Errors
    /// propagated
    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        Read::read_to_end(&mut &*self, buf)
    }

    /// See `Read::read_to_string`
    /// # Errors
    /// propagated
    pub fn read_to_string(&self, buf: &mut String) -> io::Result<usize> {
        Read::read_to_string(&mut &*self, buf)
    }

    /// See `Read::read_exact`
    /// # Errors
    /// propagated
    pub fn read_exact(&self, buf: &mut [u8]) -> io::Result<()> {
        Read::read_exact(&mut &*self, buf)
    }

    /// See `Write::write_all`
    /// # Errors
    /// propagated
    pub fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        Write::write_all(&mut &*self, buf)
    }

    /// See `Write::write_fmt`
    /// # Errors
    /// propagated
    pub fn write_fmt(&self, fmt: Arguments<'_>) -> io::Result<()> {
        Write::write_fmt(&mut &*self, fmt)
    }
}

impl<C, S> Read for RustTlsDuplexStream<C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>> + Send,
    S: rustls::SideData,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Self::read(self, buf)
    }
}

impl<C, S> Read for &RustTlsDuplexStream<C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>> + Send,
    S: rustls::SideData,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        RustTlsDuplexStream::<C, S>::read(self, buf)
    }
}

impl<C, S> Write for RustTlsDuplexStream<C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>> + Send,
    S: rustls::SideData,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Self::write(self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Self::flush(self)
    }
}

impl<C, S> Write for &RustTlsDuplexStream<C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>> + Send,
    S: rustls::SideData,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        RustTlsDuplexStream::<C, S>::write(self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        RustTlsDuplexStream::<C, S>::flush(self)
    }
}

/// Read+Write combiner that is fed into rust-tls and delegates to our special ReadPipe/WritePipe
/// that have careful blocking semantics
#[derive(Debug)]
struct CombinedPipe(ReadPipe, WritePipe);

impl CombinedPipe {
    ///Constructor for `CombinedPipe`
    pub fn new<
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        T: FnMut(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    >(
        read: R,
        write: W,
        mut spawner: T,
    ) -> io::Result<Self> {
        Ok(Self(
            ReadPipe::new(read, &mut spawner)?,
            WritePipe::new(write, &mut spawner)?,
        ))
    }
}

impl Read for CombinedPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for CombinedPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.1.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.1.flush()
    }
}

/// Poison error to `io::Error`
pub(crate) fn unwrap_poison<T>(result: LockResult<T>) -> io::Result<T> {
    result.map_err(|_| io::Error::new(ErrorKind::Other, "Poisoned Mutex"))
}
