use std::cell::UnsafeCell;
use std::future::Future;
use std::ops::{Deref, DerefMut, Drop};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::task::{Context, Poll};

use crate::waiter::Waiter;
use cordyceps::List;

pub struct RwLock<T> {
    /// The number of read locks times two, plus one if there's a writer waiting.
    /// u32::MAX if write-locked.
    ///
    /// This means that readers may acquire the lock when
    /// the state is even, but need to block when odd.
    state: AtomicU32,
    value: UnsafeCell<T>,
    writer_waiters: SyncMutex<List<Waiter>>,
    reader_waiters: SyncMutex<List<Waiter>>,
}

unsafe impl<T> Sync for RwLock<T> where T: Send + Sync {}

impl<T> RwLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0), // unlocked
            value: UnsafeCell::new(value),
            writer_waiters: SyncMutex::new(List::new()),
            reader_waiters: SyncMutex::new(List::new()),
        }
    }

    pub fn read(&self) -> ReadLockFuture<'_, T> {
        ReadLockFuture {
            rwlock: self,
            waiter: Waiter::default(),
        }
    }

    pub fn write(&self) -> WriteLockFuture<'_, T> {
        WriteLockFuture {
            rwlock: self,
            waiter: Waiter::default(),
        }
    }
}

pub struct WriteLockFuture<'a, T> {
    rwlock: &'a RwLock<T>,
    waiter: Waiter,
}

impl<'a, T> Drop for WriteLockFuture<'a, T> {
    fn drop(&mut self) {
        // this process is needed to eliminate stale wakers in the writer wakers queue
        let mut w_guard = self.rwlock.writer_waiters.lock().unwrap();
        if self.waiter.waker.is_some() {
            unsafe { w_guard.remove(NonNull::from_ref(&self.waiter)) };
        }

        if w_guard.is_empty() {
            // There's no more waiting writers, clear the odd bit and wake the readers
            let mut s = self.rwlock.state.load(Relaxed);
            while !s.is_multiple_of(2) && s != u32::MAX {
                match self
                    .rwlock
                    .state
                    .compare_exchange_weak(s, s - 1, Release, Relaxed)
                {
                    Ok(_) => break,
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }
            if s != u32::MAX {
                drop(w_guard);
                let mut r_guard = self.rwlock.reader_waiters.lock().unwrap();
                let mut wakers = Vec::new();
                while let Some(ptr) = r_guard.pop_front() {
                    let w = unsafe { (*ptr.as_ptr()).waker.take().unwrap() };
                    wakers.push(w);
                }
                drop(r_guard);
                wakers.into_iter().for_each(|w| w.wake());
            }
        }
    }
}

impl<'a, T> Future for WriteLockFuture<'a, T> {
    type Output = WriteGuard<'a, T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Safety: We need to make sure no pointers are moved around.
        let this = unsafe { self.get_unchecked_mut() };
        let mut s = this.rwlock.state.load(Relaxed);

        loop {
            // try to lock if unlocked
            if s <= 1 {
                match this
                    .rwlock
                    .state
                    .compare_exchange_weak(s, u32::MAX, Acquire, Relaxed)
                {
                    Ok(_) => {
                        return Poll::Ready(WriteGuard {
                            rwlock: this.rwlock,
                        });
                    }
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }

            // block new readers by making sure the state is odd
            if s.is_multiple_of(2)
                && let Err(e) = this
                    .rwlock
                    .state
                    .compare_exchange_weak(s, s + 1, Relaxed, Relaxed)
            {
                s = e;
                continue;
            }

            // wait if it's still locked
            let mut w_wakers = this.rwlock.writer_waiters.lock().unwrap();
            let needs_push = this.waiter.waker.is_none();
            this.waiter.add_waker(cx.waker().clone());
            if needs_push {
                w_wakers.push_back(NonNull::from_ref(&this.waiter));
            }

            // double check to see if we can grab the lock
            s = this.rwlock.state.load(Relaxed);
            if s <= 1 {
                // lock became free
                // Safety: We know this waiter is in the list
                let _ = unsafe { w_wakers.remove(NonNull::from_ref(&this.waiter)) };
                let _ = this.waiter.waker.take();
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
    waiter: Waiter,
}

impl<'a, T> Drop for ReadLockFuture<'a, T> {
    fn drop(&mut self) {
        // remove the waker associated with this Future if it drops prior to completion
        let mut r_guard = self.rwlock.reader_waiters.lock().unwrap();
        if self.waiter.waker.is_some() {
            unsafe { r_guard.remove(NonNull::from_ref(&self.waiter)) };
        }
    }
}

impl<'a, T> Future for ReadLockFuture<'a, T> {
    type Output = ReadGuard<'a, T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Safety: don't move any pointers around!
        let this = unsafe { self.get_unchecked_mut() };
        let mut s = this.rwlock.state.load(Relaxed);
        loop {
            if s.is_multiple_of(2) {
                // even, free to take the lock
                assert!(s != u32::MAX - 2, "too many readers");
                match this
                    .rwlock
                    .state
                    .compare_exchange_weak(s, s + 2, Acquire, Relaxed)
                {
                    Ok(_) => {
                        return Poll::Ready(ReadGuard {
                            rwlock: this.rwlock,
                        });
                    }
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }

            // state is odd, writer waiting or write-locked
            let mut r_guard = this.rwlock.reader_waiters.lock().unwrap();

            let needs_push = this.waiter.waker.is_none();
            this.waiter.add_waker(cx.waker().clone()); // always update the waker
            if needs_push {
                r_guard.push_back(NonNull::from_ref(&this.waiter));
            }

            // double check to see if the lock became available
            s = this.rwlock.state.load(Relaxed);
            if s.is_multiple_of(2) {
                let _ = unsafe { r_guard.remove(NonNull::from_ref(&this.waiter)) };
                let _ = this.waiter.waker.take();
                drop(r_guard);
                continue; // retry acquisition
            }
            drop(r_guard);
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
            let mut w_guard = self.rwlock.writer_waiters.lock().unwrap();
            if let Some(ptr) = w_guard.pop_front() {
                let waker = unsafe { (*ptr.as_ptr()).waker.take() };
                waker.unwrap().wake();
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
        let mut w_guard = self.rwlock.writer_waiters.lock().unwrap();
        if let Some(ptr) = w_guard.pop_front() {
            // we have a waiting writer
            self.rwlock.state.store(1, Release); // update state to 1
            let waker = unsafe { (*ptr.as_ptr()).waker.take().unwrap() };
            drop(w_guard);
            waker.wake();
            return;
        }
        // no waiting writers, time to wake all the readers
        self.rwlock.state.store(0, Release); // update state to 0 - unlocked
        drop(w_guard); // THEN drop the w_guard to make sure nobody grabbed it before the state change

        let mut r_guard = self.rwlock.reader_waiters.lock().unwrap();
        let mut wakers = Vec::new();

        while let Some(ptr) = r_guard.pop_front() {
            let w = unsafe { (*ptr.as_ptr()).waker.take().unwrap() };
            wakers.push(w);
        }
        drop(r_guard);
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
