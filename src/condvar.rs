use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::task::{Poll, Waker};
use std::future::Future;
use crossbeam_queue::SegQueue;

use crate::mutex::MutexGuard;

pub struct Condvar {
    wakers: SegQueue<Waker>,
}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            wakers: SegQueue::new(),
        }
    }

    pub fn notify_one(&self) {
        if let Some(w) = self.wakers.pop() {
            w.wake();
        }
    }

    pub fn notify_all(&self) {
        while let Some(w) = self.wakers.pop() {
            w.wake();
        }
    }

    pub fn wait<'a, 'b, T>(&'a self, guard: MutexGuard<'b, T>) -> CondvarFuture<'a, 'b, T> {
        // self.num_waiters.fetch_add(1, Relaxed);
        // let counter_value = self.counter.load(Relaxed);

        // // Unlock the mutex by dropping the guard,
        // // but remember the mutex so we can lock it again later.
        // let mutex = guard.mutex;
        // drop(guard);

        // // Wait, but only if the counter hasn't changed since unlocking.
        // wait(&self.counter, counter_value);

        // self.num_waiters.fetch_sub(1, Relaxed);

        // mutex.lock()
        CondvarFuture { guard, cv: self }
    }
}

struct CondvarFuture<'a, 'b, T> {
    cv: &'a Condvar,
    guard: MutexGuard<'b, T>,
}

impl<'a, 'b, T> Future for CondvarFuture<'a, 'b, T> {
    type Output = MutexGuard<'b, T>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mutex = self.guard.mutex;
        Poll::Pending
    }
}
