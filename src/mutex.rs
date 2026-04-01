use cordyceps::List;
use std::cell::UnsafeCell;
use std::future::Future;
use std::ops::{Deref, DerefMut, Drop};
use std::pin::Pin;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::task::{Context, Poll};

use std::ptr::NonNull;

use crate::waiter::Waiter;

pub struct Mutex<T> {
    /// false: Unlocked
    /// true: Locked
    state: AtomicBool,
    value: UnsafeCell<T>,
    // Needed a SyncMutex becauase I had a race condition
    // in drop() where the waiters list was still empty so nobody got
    // woken up
    waiters: SyncMutex<List<Waiter>>,
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            state: AtomicBool::new(false), // starts unlocked
            value: UnsafeCell::new(value),
            waiters: SyncMutex::new(List::new()),
        }
    }

    pub fn lock(&self) -> LockFuture<'_, T> {
        LockFuture {
            mutex: self,
            waiter: Waiter::default(),
        }
    }
}

unsafe impl<T> Sync for Mutex<T> where T: Send {}

pub struct MutexGuard<'a, T> {
    pub(crate) mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // hold the lock while releasing state AND waking
        let mut waiter_guard = self.mutex.waiters.lock().unwrap();
        self.mutex.state.store(false, Release);
        if let Some(ptr) = waiter_guard.pop_front() {
            let waker = unsafe { (*ptr.as_ptr()).waker.take().unwrap() };
            waker.wake();
        }
    }
}

pub struct LockFuture<'a, T> {
    mutex: &'a Mutex<T>,
    waiter: Waiter,
}

impl<'a, T> Drop for LockFuture<'a, T> {
    fn drop(&mut self) {
        // remove the waker associated with this Future if it drops prior to completion
        let mut waiter_guard = self.mutex.waiters.lock().unwrap();
        if self.waiter.waker.is_some() {
            let _ = unsafe { waiter_guard.remove(NonNull::from_ref(&self.waiter)) };
        }
    }
}

impl<'a, T> Future for LockFuture<'a, T> {
    type Output = MutexGuard<'a, T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // try to atomically acquire the lock
        // false = unlocked, attempt to swap to true = locked
        if self
            .mutex
            .state
            .compare_exchange(false, true, Acquire, Relaxed)
            .is_ok()
        {
            // we got it
            return Poll::Ready(MutexGuard { mutex: self.mutex });
        }

        // contended - register waker
        let this = unsafe { self.get_unchecked_mut() };
        let mut waiters = this.mutex.waiters.lock().unwrap();

        let needs_push = this.waiter.waker.is_none();
        this.waiter.add_waker(cx.waker().clone()); // always update the waker
        if needs_push {
            waiters.push_back(NonNull::from_ref(&this.waiter));
        }

        // double check before returning pending
        if this
            .mutex
            .state
            .compare_exchange(false, true, Acquire, Relaxed)
            .is_ok()
        {
            // Safety: We know this waiter is in the list
            unsafe { waiters.remove(NonNull::from_ref(&this.waiter)) };
            let _ = this.waiter.waker.take(); // not really required since we're about to drop anyhow
            return Poll::Ready(MutexGuard { mutex: this.mutex });
        }
        // Ok - we lost, park this task
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn test_async_mutex_contended() {
        let num_tasks = 5;
        let increments_per_task = 5;

        let mutex = Arc::new(Mutex::new(0usize));
        let completed = Arc::new(AtomicUsize::new(0));

        let ex = Arc::new(smol::Executor::new());

        // spawn N OS threads all running the same executor
        let _threads: Vec<_> = (0..8)
            .map(|_| {
                let ex = Arc::clone(&ex);
                std::thread::spawn(move || smol::block_on(ex.run(std::future::pending::<()>())))
            })
            .collect();

        // spawn tasks onto the shared executor
        smol::block_on(ex.run(async {
            let tasks: Vec<_> = (0..num_tasks)
                .map(|id| {
                    let mutex = Arc::clone(&mutex);
                    let completed = Arc::clone(&completed);
                    ex.spawn(async move {
                        for _ in 0..increments_per_task {
                            let mut guard = mutex.lock().await;
                            *guard += 1;
                            println!("[{id}] counter = {}", *guard);
                            drop(guard);
                            smol::future::yield_now().await;
                        }
                        completed.fetch_add(1, Relaxed);
                        println!("[task {id}] finished");
                    })
                })
                .collect();

            futures::future::join_all(tasks).await;
        }));

        let final_count = *smol::block_on(mutex.lock());
        assert_eq!(final_count, num_tasks * increments_per_task);
        assert_eq!(completed.load(Relaxed), num_tasks);
        println!("final counter value: {final_count}");
    }
}
