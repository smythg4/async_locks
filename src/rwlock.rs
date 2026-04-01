use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::future::Future;
use std::ops::{Deref, DerefMut, Drop};
use std::pin::Pin;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU32, AtomicUsize};
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
    writer_wakers: SyncMutex<HashMap<usize, Waker>>,
    reader_wakers: SyncMutex<HashMap<usize, Waker>>,
    next_waiter_id: AtomicUsize,
}

unsafe impl<T> Sync for RwLock<T> where T: Send + Sync {}

impl<T> RwLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0), // unlocked
            value: UnsafeCell::new(value),
            writer_wakers: SyncMutex::new(HashMap::new()),
            reader_wakers: SyncMutex::new(HashMap::new()),
            next_waiter_id: AtomicUsize::new(0),
        }
    }

    pub fn read(&self) -> ReadLockFuture<'_, T> {
        ReadLockFuture {
            rwlock: self,
            slot_id: self.next_slot_id(),
        }
    }

    pub fn write(&self) -> WriteLockFuture<'_, T> {
        WriteLockFuture {
            rwlock: self,
            slot_id: self.next_slot_id(),
        }
    }

    fn next_slot_id(&self) -> usize {
        self.next_waiter_id.fetch_add(1, Relaxed)
    }
}

pub struct WriteLockFuture<'a, T> {
    rwlock: &'a RwLock<T>,
    slot_id: usize,
}

impl<'a, T> Drop for WriteLockFuture<'a, T> {
    fn drop(&mut self) {
        // this process is needed to eliminate stale wakers in the writer wakers queue
        let mut w_guard = self.rwlock.writer_wakers.lock().unwrap();
        let _ = w_guard.remove(&self.slot_id);
        if w_guard.is_empty() {
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
            while let Some(id) = r_guard.keys().next().copied() {
                let w = r_guard.remove(&id).unwrap();
                wakers.push(w);
            }
            drop(r_guard);
            // wake ALL the readers
            wakers.into_iter().for_each(|w| w.wake());
        }
    }
}

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
            if let Some(w) = w_wakers.get_mut(&self.slot_id) {
                w.clone_from(cx.waker()); // update stale waker, re-park
            } else {
                w_wakers.insert(self.slot_id, cx.waker().clone());
            }

            s = self.rwlock.state.load(Relaxed);
            if s <= 1 {
                // lock became free
                let _ = w_wakers.remove(&self.slot_id);
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
    slot_id: usize,
}

impl<'a, T> Drop for ReadLockFuture<'a, T> {
    fn drop(&mut self) {
        // remove the waker associated with this Future if it drops prior to completion
        let _ = self
            .rwlock
            .reader_wakers
            .lock()
            .unwrap()
            .remove(&self.slot_id);
    }
}

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

            if let Some(w) = r_wakers.get_mut(&self.slot_id) {
                w.clone_from(cx.waker()); // update stale waker, re-park
            } else {
                r_wakers.insert(self.slot_id, cx.waker().clone());
            }

            s = self.rwlock.state.load(Relaxed);
            if s.is_multiple_of(2) {
                let _ = r_wakers.remove(&self.slot_id);
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
            if let Some(id) = w_guard.keys().next().copied() {
                w_guard.remove(&id).unwrap().wake();
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
        if let Some(id) = w_guard.keys().next().copied() {
            let w = w_guard.remove(&id).unwrap();
            self.rwlock.state.store(1, Release);
            drop(w_guard);
            w.wake();
            return;
        }
        self.rwlock.state.store(0, Release);
        drop(w_guard);

        let mut r_guard = self.rwlock.reader_wakers.lock().unwrap();
        let mut wakers = Vec::new();
        while let Some(id) = r_guard.keys().next().copied() {
            let w = r_guard.remove(&id).unwrap();
            wakers.push(w);
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
