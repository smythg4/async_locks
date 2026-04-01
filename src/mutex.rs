use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::future::Future;
use std::ops::{Deref, DerefMut, Drop};
use std::pin::Pin;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::task::Waker;
use std::task::{Context, Poll};

pub struct Mutex<T> {
    /// false: Unlocked
    /// true: Locked
    state: AtomicBool,
    value: UnsafeCell<T>,
    // Needed a SyncMutex becauase I had a race condition
    // in drop() where the waiters list was still empty so nobody got
    // woken up
    waiters: SyncMutex<HashMap<usize, Waker>>,
    next_waiter_id: AtomicUsize,
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            state: AtomicBool::new(false), // starts unlocked
            value: UnsafeCell::new(value),
            waiters: SyncMutex::new(HashMap::new()),
            next_waiter_id: AtomicUsize::new(0),
        }
    }

    pub fn lock(&self) -> LockFuture<'_, T> {
        LockFuture {
            mutex: self,
            slot_id: self.next_slot_id(),
        }
    }

    fn next_slot_id(&self) -> usize {
        self.next_waiter_id.fetch_add(1, Relaxed)
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
        let mut waiters = self.mutex.waiters.lock().unwrap();
        self.mutex.state.store(false, Release);
        if let Some(id) = waiters.keys().next().copied() {
            waiters.remove(&id).unwrap().wake();
        }
    }
}

pub struct LockFuture<'a, T> {
    mutex: &'a Mutex<T>,
    slot_id: usize,
}

impl<'a, T> Drop for LockFuture<'a, T> {
    fn drop(&mut self) {
        // remove the waker associated with this Future if it drops prior to completion
        let _ = self.mutex.waiters.lock().unwrap().remove(&self.slot_id);
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
        let mut waiters = self.mutex.waiters.lock().unwrap();
        if let Some(w) = waiters.get_mut(&self.slot_id) {
            w.clone_from(cx.waker()); // update stale waker, re-park
        } else {
            waiters.insert(self.slot_id, cx.waker().clone());
        }

        // double check before returning pending
        if self
            .mutex
            .state
            .compare_exchange(false, true, Acquire, Relaxed)
            .is_ok()
        {
            let _ = waiters.remove(&self.slot_id);
            return Poll::Ready(MutexGuard { mutex: self.mutex });
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
