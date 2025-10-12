#[cfg(not(feature = "std-mutex"))]
use crate::mutex::{Mutex, MutexGuard};
use crate::signal::{Signal, SignalTerminator};
extern crate alloc;
use alloc::collections::VecDeque;
use branches::unlikely;
#[cfg(feature = "std-mutex")]
use std::sync::{Mutex, MutexGuard};

pub(crate) struct Internal<T> {
    /// The internal channel object
    internal: *mut Mutex<ChannelInternal<T>>,
}

// Mutex is sync and send, so we can send it across threads
unsafe impl<T: Send> Send for Internal<T> {}
unsafe impl<T> Sync for Internal<T> {}

impl<T> Internal<T> {
    #[inline(always)]
    pub(crate) unsafe fn drop(&self) {
        let _ = Box::from_raw(self.internal);
    }
    /// Returns a channel internal with the required capacity
    #[inline(always)]
    pub(crate) fn new(bounded: bool, capacity: usize) -> Internal<T> {
        let mut abstract_capacity = capacity;
        if !bounded {
            // act like there is no limit
            abstract_capacity = usize::MAX;
        }
        let wait_list_size = if capacity == 0 { 8 } else { 4 };
        let ret = ChannelInternal {
            queue: VecDeque::with_capacity(capacity),
            recv_blocking: false,
            wait_list: VecDeque::with_capacity(wait_list_size),
            recv_count: 1,
            send_count: 1,
            capacity: abstract_capacity,
            // always has the default one sender and one receiver
            ref_count: 2,
        };

        Internal {
            internal: Box::into_raw(Box::new(Mutex::new(ret))),
        }
    }

    #[inline(always)]
    pub(crate) fn clone_recv(&self) -> Internal<T> {
        acquire_internal(self).inc_ref_count(false);
        Internal {
            internal: self.internal,
        }
    }

    #[inline(always)]
    pub(crate) fn drop_recv(&mut self) {
        let mut internal = acquire_internal(self);
        if unlikely(internal.dec_ref_count(false)) {
            drop(internal);
            unsafe { self.drop() }
        }
    }

    #[inline(always)]
    pub(crate) fn clone_send(&self) -> Internal<T> {
        acquire_internal(self).inc_ref_count(true);
        Internal {
            internal: self.internal,
        }
    }

    #[inline(always)]
    pub(crate) fn drop_send(&mut self) {
        let mut internal = acquire_internal(self);
        if unlikely(internal.dec_ref_count(true)) {
            drop(internal);
            unsafe { self.drop() }
        }
    }

    #[inline(always)]
    pub(crate) fn clone_unchecked(&self) -> Internal<T> {
        Internal {
            internal: self.internal,
        }
    }
}

impl<T> core::ops::Deref for Internal<T> {
    type Target = Mutex<ChannelInternal<T>>;
    fn deref(&self) -> &Self::Target {
        unsafe { &(*self.internal) }
    }
}

/// Acquire mutex guard on channel internal for use in channel operations
#[inline(always)]
pub(crate) fn acquire_internal<T>(internal: &'_ Internal<T>) -> MutexGuard<'_, ChannelInternal<T>> {
    #[cfg(not(feature = "std-mutex"))]
    return internal.lock();
    #[cfg(feature = "std-mutex")]
    internal.lock().unwrap_or_else(|err| err.into_inner())
}

/// Tries to acquire mutex guard on channel internal for use in channel
/// operations
#[inline(always)]
pub(crate) fn try_acquire_internal<T>(
    internal: &'_ Internal<T>,
) -> Option<MutexGuard<'_, ChannelInternal<T>>> {
    #[cfg(not(feature = "std-mutex"))]
    return internal.try_lock();
    #[cfg(feature = "std-mutex")]
    internal.try_lock().ok()
}

/// Internal of the channel that holds queues, waitlists, and general state of
/// the channel, it's shared among senders and receivers with an atomic
/// counter and a mutex
pub(crate) struct ChannelInternal<T> {
    // KEEP THE ORDER
    /// Channel queue to save buffered objects
    pub(crate) queue: VecDeque<T>,
    /// It's true if the signals in the waiting list are recv signals
    pub(crate) recv_blocking: bool,
    /// Receive and Send waitlist for when the channel queue is empty or zero
    /// capacity for recv or full for send.
    pub(crate) wait_list: VecDeque<SignalTerminator<T>>,
    /// The capacity of the channel buffer
    pub(crate) capacity: usize,
    /// Count of alive receivers
    pub(crate) recv_count: u32,
    /// Count of alive senders
    pub(crate) send_count: u32,
    /// Reference counter for the alive instances of the channel
    pub(crate) ref_count: u64,
}

impl<T> ChannelInternal<T> {
    /// Terminates remainings signals in the queue to notify listeners about the
    /// closing of the channel
    #[cold]
    pub(crate) fn terminate_signals(&mut self) {
        for t in self.wait_list.iter() {
            // Safety: it's safe to terminate owned signal once
            unsafe { t.terminate() }
        }
        self.wait_list.clear();
    }

    /// Returns next signal for sender from the waitlist
    #[inline(always)]
    pub(crate) fn next_send(&mut self) -> Option<SignalTerminator<T>> {
        if self.recv_blocking {
            return None;
        }
        match self.wait_list.pop_front() {
            Some(sig) => Some(sig),
            None => {
                self.recv_blocking = true;
                None
            }
        }
    }

    /// Adds new sender signal to the waitlist
    #[inline(always)]
    pub(crate) fn push_send(&mut self, s: SignalTerminator<T>) {
        self.wait_list.push_back(s);
    }

    /// Returns the next signal for the receiver in the waitlist
    #[inline(always)]
    pub(crate) fn next_recv(&mut self) -> Option<SignalTerminator<T>> {
        if !self.recv_blocking {
            return None;
        }
        match self.wait_list.pop_front() {
            Some(sig) => Some(sig),
            None => {
                self.recv_blocking = false;
                None
            }
        }
    }

    /// Adds new receiver signal to the waitlist
    #[inline(always)]
    pub(crate) fn push_recv(&mut self, s: SignalTerminator<T>) {
        self.wait_list.push_back(s);
    }

    /// Tries to remove the send signal from the waitlist, returns true if the
    /// operation was successful
    pub(crate) fn cancel_send_signal(&mut self, sig: &Signal<T>) -> bool {
        if !self.recv_blocking {
            for (i, send) in self.wait_list.iter().enumerate() {
                if send.eq(sig) {
                    self.wait_list.remove(i);
                    return true;
                }
            }
        }
        false
    }

    /// Tries to remove the received signal from the waitlist, returns true if
    /// the operation was successful
    pub(crate) fn cancel_recv_signal(&mut self, sig: &Signal<T>) -> bool {
        if self.recv_blocking {
            for (i, recv) in self.wait_list.iter().enumerate() {
                if recv.eq(sig) {
                    self.wait_list.remove(i);
                    return true;
                }
            }
        }
        false
    }

    /// checks if send signal exists in wait list
    #[cfg(feature = "async")]
    pub(crate) fn send_signal_exists(&self, sig: &Signal<T>) -> bool {
        if !self.recv_blocking {
            for signal in self.wait_list.iter() {
                if signal.eq(sig) {
                    return true;
                }
            }
        }
        false
    }

    /// checks if receive signal exists in wait list
    #[cfg(feature = "async")]
    pub(crate) fn recv_signal_exists(&self, sig: &Signal<T>) -> bool {
        if self.recv_blocking {
            for signal in self.wait_list.iter() {
                if signal.eq(sig) {
                    return true;
                }
            }
        }
        false
    }

    /// Increases ref count for sender or receiver
    #[inline(always)]
    pub(crate) fn inc_ref_count(&mut self, is_sender: bool) {
        if is_sender {
            if self.send_count > 0 {
                self.send_count += 1;
            }
        } else if self.recv_count > 0 {
            self.recv_count += 1;
        }
        self.ref_count += 1;
    }

    /// Decreases ref count for sender or receiver
    #[inline(always)]
    pub(crate) fn dec_ref_count(&mut self, is_sender: bool) -> bool {
        if is_sender {
            if self.send_count > 0 {
                self.send_count -= 1;
                if self.send_count == 0 && self.recv_count != 0 {
                    self.terminate_signals();
                }
            }
        } else if self.recv_count > 0 {
            self.recv_count -= 1;
            if self.recv_count == 0 && self.send_count != 0 {
                self.terminate_signals();
            }
        }
        self.ref_count -= 1;
        self.ref_count == 0
    }
}
