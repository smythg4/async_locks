use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::future::Future;
use std::ops::{Deref, DerefMut, Drop};
use std::pin::Pin;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::{Arc, Mutex as SyncMutex};
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
    writer_wakers: SyncMutex<VecDeque<Arc<SyncMutex<Option<Waker>>>>>,
    reader_wakers: SyncMutex<VecDeque<Arc<SyncMutex<Option<Waker>>>>>,
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
        ReadLockFuture {
            rwlock: self,
            slot: None,
        }
    }

    pub fn write(&self) -> WriteLockFuture<'_, T> {
        WriteLockFuture {
            rwlock: self,
            slot: None,
        }
    }
}

pub struct WriteLockFuture<'a, T> {
    rwlock: &'a RwLock<T>,
    slot: Option<Arc<SyncMutex<Option<Waker>>>>,
}

impl<'a, T> Drop for WriteLockFuture<'a, T> {
    fn drop(&mut self) {
        // this process is needed to eliminate stale wakers in the writer wakers queue
        if let Some(slot) = self.slot.take() {
            *slot.lock().unwrap() = None;
            let w_guard = self.rwlock.writer_wakers.lock().unwrap();
            let all_writers_dead = w_guard.iter().all(|s| s.lock().unwrap().is_none());
            if all_writers_dead {
                // clear the odd bit - there are no writers waiting
                let mut s = self.rwlock.state.load(Relaxed);
                loop {
                    if s.is_multiple_of(2) || s == u32::MAX {
                        break;
                    }
                    match self
                        .rwlock
                        .state
                        .compare_exchange_weak(s, s - 1, Release, Relaxed)
                    {
                        Ok(_) => {
                            break;
                        }
                        Err(e) => {
                            s = e;
                        }
                    };
                }
                drop(w_guard);
                // wake the readers - there are no writers waiting
                let mut r_guard = self.rwlock.reader_wakers.lock().unwrap();
                let mut wakers = Vec::new();
                while let Some(g) = r_guard.pop_front() {
                    if let Some(w) = g.lock().unwrap().take() {
                        // collect all the reader wakers
                        wakers.push(w);
                    }
                }
                drop(r_guard);
                // wake ALL the readers
                wakers.into_iter().for_each(|w| w.wake());
            }
        }
    }
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
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
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
            let slot = Arc::new(SyncMutex::new(Some(cx.waker().clone())));
            self.slot = Some(Arc::clone(&slot));
            w_wakers.push_back(slot);

            s = self.rwlock.state.load(Relaxed);
            if s <= 1 {
                // lock became free
                let _ = w_wakers.pop_back();
                let _ = self.slot.take();
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
    slot: Option<Arc<SyncMutex<Option<Waker>>>>,
}

impl<'a, T> Drop for ReadLockFuture<'a, T> {
    fn drop(&mut self) {
        // this process is needed to eliminate stale wakers in the reader wakers queue
        if let Some(slot) = self.slot.take() {
            *slot.lock().unwrap() = None;
        }
    }
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
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
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

            let slot = Arc::new(SyncMutex::new(Some(cx.waker().clone())));
            self.slot = Some(Arc::clone(&slot));
            r_wakers.push_back(slot);

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
            while let Some(g) = w_guard.pop_front() {
                if let Some(w) = g.lock().unwrap().take() {
                    drop(w_guard);
                    w.wake();
                    break;
                }
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
        while let Some(g) = w_guard.pop_front() {
            if let Some(w) = g.lock().unwrap().take() {
                self.rwlock.state.store(1, Release);
                drop(w_guard);
                // wake one writer
                w.wake();
                return;
            }
        }
        drop(w_guard);

        let mut r_guard = self.rwlock.reader_wakers.lock().unwrap();
        self.rwlock.state.store(0, Release);
        let mut wakers = Vec::new();
        while let Some(g) = r_guard.pop_front() {
            if let Some(w) = g.lock().unwrap().take() {
                // collect all the reader wakers
                wakers.push(w);
            }
        }
        drop(r_guard);
        // wake ALL the readers
        wakers.into_iter().for_each(|w| w.wake());
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
        let num_writers = 3;
        let num_readers = 5;
        let writes_per_writer = 3;
        let reads_per_reader = 3;

        let ex = Arc::new(smol::Executor::new());

        // spawn N OS threads all running the same executor
        let _threads: Vec<_> = (0..8)
            .map(|_| {
                let ex = Arc::clone(&ex);
                std::thread::spawn(move || smol::block_on(ex.run(std::future::pending::<()>())))
            })
            .collect();

        smol::block_on(async move {
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
                let all_tasks: Vec<_> = vec![write_tasks, read_tasks]
                    .into_iter()
                    .flatten()
                    .collect();
                future::join_all(all_tasks).await;

                let final_val = *lock.read().await;
                assert_eq!(final_val, num_writers * writes_per_writer);
            })
            .await;
        });
    }
}
