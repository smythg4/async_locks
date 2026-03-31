use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::future::Future;
use std::ops::{Deref, DerefMut, Drop};
use std::pin::Pin;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::task::Waker;
use std::task::{Context, Poll};

pub struct RwLock<T> {
    /// The number of read locks times two, plus one if there's a writer waiting.
    /// u32::MAX if write-locked.
    ///
    /// This means that readers may acquire the lock when
    /// the state is even, but need to block when odd.
    state: AtomicU32,
    value: UnsafeCell<T>,
    writer_wakers: SyncMutex<VecDeque<Waker>>,
    reader_wakers: SyncMutex<VecDeque<Waker>>,
}

unsafe impl<T> Sync for RwLock<T> where T: Send + Sync {}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0), // unlocked
            value: UnsafeCell::new(value),
            writer_wakers: SyncMutex::new(VecDeque::new()),
            reader_wakers: SyncMutex::new(VecDeque::new()),
        }
    }

    pub fn read(&self) -> ReadLockFuture<'_, T> {
        ReadLockFuture { rwlock: self }
    }

    pub fn write(&self) -> WriteLockFuture<'_, T> {
        WriteLockFuture { rwlock: self }
    }
}

pub struct WriteLockFuture<'a, T> {
    rwlock: &'a RwLock<T>,
}
// sync version:
// let mut s = self.state.load(Relaxed);
// loop {
//     // try to lock if unlocked
//     if s <= 1 {
//         match self.state.compare_exchange(s, u32::MAX, Acquire, Relaxed) {
//             Ok(_) => return WriteGuard { rwlock: self },
//             Err(e) => {
//                 s = e;
//                 continue;
//             }
//         }
//     }
//     // block new readers, by making sure the state is odd
//     if s.is_multiple_of(2) {
//         match self.state.compare_exchange(s, s + 1, Relaxed, Relaxed) {
//             Ok(_) => {}
//             Err(e) => {
//                 s = e;
//                 continue;
//             }
//         }
//     }
//     // Wait if it's still locked
//     let w = self.writer_wake_counter.load(Acquire);
//     s = self.state.load(Relaxed);
//     if s >= 2 {
//         wait(&self.writer_wake_counter, w);
//         s = self.state.load(Relaxed);
//     }
// }

impl<'a, T> Future for WriteLockFuture<'a, T> {
    type Output = WriteGuard<'a, T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut s = self.rwlock.state.load(Relaxed);
        loop {
            // try to lock if unlocked
            if s <= 1 {
                match self
                    .rwlock
                    .state
                    .compare_exchange_weak(s, u32::MAX, Acquire, Relaxed)
                {
                    Ok(_) => {
                        return Poll::Ready(WriteGuard {
                            rwlock: self.rwlock,
                        });
                    }
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }

            // block new readers, by making sure the state is odd
            if s.is_multiple_of(2)
                && let Err(e) = self
                    .rwlock
                    .state
                    .compare_exchange_weak(s, s + 1, Relaxed, Relaxed)
            {
                s = e;
                continue;
            }

            // wait if it's still locked
            let mut w_wakers = self.rwlock.writer_wakers.lock().unwrap();
            w_wakers.push_back(cx.waker().clone());
            s = self.rwlock.state.load(Relaxed);
            if s <= 1 {
                // lock became free
                let _ = w_wakers.pop_back();
                drop(w_wakers);
                continue; // retry acquisition
            }
            drop(w_wakers);
            // lock still held
            return Poll::Pending;
        }
    }
}

pub struct ReadLockFuture<'a, T> {
    rwlock: &'a RwLock<T>,
}
// sync version:
// let mut s = self.state.load(Relaxed);
// loop {
//     if s.is_multiple_of(2) {
//         // Even, free to take the lock!
//         assert!(s != u32::MAX - 2, "too many readers");
//         match self.state.compare_exchange_weak(s, s + 2, Acquire, Relaxed) {
//             Ok(_) => return ReadGuard { rwlock: self },
//             Err(e) => s = e,
//         }
//     }
//     if !s.is_multiple_of(2) {
//         // Odd, there's a waiting writer, defer to it.
//         wait(&self.state, s);
//         s = self.state.load(Relaxed);
//     }
// }

impl<'a, T> Future for ReadLockFuture<'a, T> {
    type Output = ReadGuard<'a, T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut s = self.rwlock.state.load(Relaxed); // why is relaxed ok here? Ahh, because the compare_exchange is Acquire
        loop {
            if s.is_multiple_of(2) {
                // even, free to take the lock
                assert!(s != u32::MAX - 2, "too many readers");
                match self
                    .rwlock
                    .state
                    .compare_exchange_weak(s, s + 2, Acquire, Relaxed)
                {
                    Ok(_) => {
                        return Poll::Ready(ReadGuard {
                            rwlock: self.rwlock,
                        });
                    }
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }
            // odd, writer waiting or write-locked
            let mut r_wakers = self.rwlock.reader_wakers.lock().unwrap();
            r_wakers.push_back(cx.waker().clone());
            s = self.rwlock.state.load(Relaxed);
            if s.is_multiple_of(2) {
                let _ = r_wakers.pop_back();
                drop(r_wakers);
                continue; // retry acquisition
            }
            drop(r_wakers);
            // still odd
            return Poll::Pending;
        }
    }
}

pub struct ReadGuard<'a, T> {
    rwlock: &'a RwLock<T>,
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.rwlock.value.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        // Decrement the state by 2 to remove one read-lock
        if self.rwlock.state.fetch_sub(2, Release) == 3 {
            // if we decrement from 3 to 1, this means
            // the RwLock is now unlocked *and* there is
            // a waiting writer, which we wake up
            let mut w_guard = self.rwlock.writer_wakers.lock().unwrap();
            if let Some(w) = w_guard.pop_front() {
                w.wake();
            }
        }
    }
}

pub struct WriteGuard<'a, T> {
    rwlock: &'a RwLock<T>,
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.rwlock.value.get() }
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.rwlock.value.get() }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        let mut w_guard = self.rwlock.writer_wakers.lock().unwrap();
        let mut r_guard = self.rwlock.reader_wakers.lock().unwrap();
        
        if let Some(w) = w_guard.pop_front() {
            self.rwlock.state.store(1, Release);
            // wake one writer
            w.wake();
            return;
        }
        self.rwlock.state.store(0, Release);
        while let Some(w) = r_guard.pop_front() {
            // wake ALL readers
            w.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use std::sync::Arc;

    #[test]
    fn test_async_rwlock() {
        let lock = Arc::new(RwLock::new(0u32));
        let num_writers = 5;
        let num_readers = 12;
        let writes_per_writer = 50;
        let reads_per_reader = 50;

        smol::block_on(async move {
            let ex = smol::Executor::new();

            let write_tasks: Vec<_> = (0..num_writers)
                .map(|id| {
                    let lock = Arc::clone(&lock);
                    async move {
                        for _ in 0..writes_per_writer {
                            let mut guard = lock.write().await;
                            *guard += 1;
                            println!("[W{id}] wrote, value is now {}", *guard);
                            drop(guard);
                            smol::future::yield_now().await;
                        }
                    }
                })
                .map(|f| ex.spawn(f))
                .collect();

            let read_tasks: Vec<_> = (0..num_readers)
                .map(|id| {
                    let lock = Arc::clone(&lock);
                    async move {
                        for _ in 0..reads_per_reader {
                            let guard = lock.read().await;
                            println!("[R{id}] read value: {}", *guard);
                            drop(guard);
                            smol::future::yield_now().await;
                        }
                    }
                })
                .map(|f| ex.spawn(f))
                .collect();

            ex.run(async {
                let all_tasks: Vec<_> = vec![write_tasks, read_tasks].into_iter().flatten().collect();
                future::join_all(all_tasks).await;

                let final_val = *lock.read().await;
                assert_eq!(final_val, num_writers * writes_per_writer);
            })
            .await;
        });
    }
}
