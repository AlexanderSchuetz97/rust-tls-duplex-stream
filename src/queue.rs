//! Poor man's channel with quirks.
use crate::unwrap_poison;
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Max size of elements in the channel
const HIGH_WATERMARK: usize = 8096;

/// Max size of elements in the channel until we only allow rust tls control messages to be queued and not actual user data.
const LOW_WATERMARK: usize = 4096;

///Poor man's channel with quirks.
#[derive(Debug, Default)]
pub struct Queue {
    /// Flag to indicate that the thread or connection has died and all surrounding it should shut down.
    dead: AtomicBool,
    /// the buffer for data an actual ring buffer would be better (and much faster) but would possibly require unsafe code.
    buffer: Mutex<VecDeque<Vec<u8>>>,
    /// Condition for when buffer changes.
    cond: Condvar,
}

impl Queue {
    
    /// Kills the queue and terminates the connection.
    pub fn kill(&self) {
        self.dead.store(true, SeqCst);
        let guard = self.buffer.lock();
        self.cond.notify_all();
        drop(guard);
    }

    /// Wait until the queue is dead, or there are less than n elements in the queue.
    fn flush_count(
        &self,
        count: usize,
        timeout: Option<Duration>,
    ) -> io::Result<MutexGuard<'_, VecDeque<Vec<u8>>>> {
        let mut guard = unwrap_poison(self.buffer.lock())?;

        while guard.len() > count {
            if self.dead.load(SeqCst) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "dead"));
            }

            if let Some(dur) = timeout.as_ref().copied() {
                let (grd, timeout) = unwrap_poison(self.cond.wait_timeout(guard, dur))?;
                if timeout.timed_out() {
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                guard = grd;
                continue;
            }

            guard = unwrap_poison(self.cond.wait(guard))?;
        }

        if self.dead.load(SeqCst) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "dead"));
        }

        Ok(guard)
    }

    /// Flush until the low watermark is reached.
    pub fn flush_low(&self, timeout: Option<Duration>) -> io::Result<()> {
        drop(self.flush_count(LOW_WATERMARK, timeout)?);
        Ok(())
    }

    /// Flush until zero elements are in the queue.
    pub fn flush_zero(&self) -> io::Result<()> {
        drop(self.flush_count(0, None)?);
        Ok(())
    }

    /// Wait until at least 1 element can be popped.
    pub fn await_pop<T>(
        &self,
        oguard: MutexGuard<'_, T>,
        duration: Option<Duration>,
    ) -> io::Result<()> {
        let mut guard = unwrap_poison(self.buffer.lock())?;
        drop(oguard);
        while guard.is_empty() {
            if self.dead.load(SeqCst) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "dead"));
            }

            if let Some(dur) = duration.as_ref().copied() {
                let (grd, timeout) = unwrap_poison(self.cond.wait_timeout(guard, dur))?;
                if timeout.timed_out() {
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                guard = grd;
                continue;
            }

            guard = unwrap_poison(self.cond.wait(guard))?;
        }

        drop(guard);
        Ok(())
    }

    /// Try to pop 1 element immediately
    pub fn try_pop(&self) -> io::Result<Option<Vec<u8>>> {
        let mut guard = unwrap_poison(self.buffer.lock())?;
        if let Some(pop) = guard.pop_front() {
            self.cond.notify_all();
            return Ok(Some(pop));
        }

        if self.dead.load(SeqCst) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "dead"));
        }

        drop(guard);
        Ok(None)
    }

    /// Blocks (forever) until 1 element could be popped or the queue is dead.
    pub fn pop(&self) -> io::Result<Vec<u8>> {
        let mut guard = unwrap_poison(self.buffer.lock())?;

        loop {
            if let Some(pop) = guard.pop_front() {
                self.cond.notify_all();
                return Ok(pop);
            }

            if self.dead.load(SeqCst) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "dead"));
            }

            guard = unwrap_poison(self.cond.wait(guard))?;
        }
    }

    /// Push 1 element onto the queue.
    pub fn push(&self, data: Vec<u8>) -> io::Result<()> {
        let mut guard = self.flush_count(HIGH_WATERMARK, None)?; //TODO not sure if None would be a better choice here. This may block control messages.
        guard.push_back(data);
        self.cond.notify_all();
        drop(guard);
        Ok(())
    }
}
