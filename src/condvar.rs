use crossbeam_queue::SegQueue;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex as SyncMutex};
use std::task::{Poll, Waker};

use crate::mutex::{Mutex, MutexGuard};

pub struct Condvar {
    wakers: SegQueue<Arc<SyncMutex<Option<Waker>>>>,
}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            wakers: SegQueue::new(),
        }
    }

    pub fn notify_one(&self) {
        while let Some(slot) = self.wakers.pop() {
            if let Some(w) = slot.lock().unwrap().take() {
                w.wake();
                break;
            }
        }
    }

    pub fn notify_all(&self) {
        while let Some(slot) = self.wakers.pop() {
            if let Some(w) = slot.lock().unwrap().take() {
                w.wake();
            }
        }
    }

    pub fn wait<'a, 'b, T>(&'a self, guard: MutexGuard<'b, T>) -> CondvarWaitFuture<'a, 'b, T> {
        CondvarWaitFuture {
            guard: Some(guard),
            cv: self,
            mutex: None,
            slot: None,
        }
    }
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CondvarWaitFuture<'a, 'b, T> {
    cv: &'a Condvar,
    guard: Option<MutexGuard<'b, T>>,
    mutex: Option<&'b Mutex<T>>,
    slot: Option<Arc<SyncMutex<Option<Waker>>>>,
}

impl<'a, 'b, T> Drop for CondvarWaitFuture<'a, 'b, T> {
    fn drop(&mut self) {
        // this process is needed to eliminate stale wakers in the Condvar waker queue
        if let Some(slot) = self.slot.take() {
            *slot.lock().unwrap() = None;
        }
    }
}

impl<'a, 'b, T> Future for CondvarWaitFuture<'a, 'b, T> {
    type Output = MutexGuard<'b, T>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        // sync version:
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

        if self.mutex.is_none() {
            // first time calling wait, register our waker, save the mutex and return pending
            let slot = Arc::new(SyncMutex::new(Some(cx.waker().clone())));
            self.cv.wakers.push(Arc::clone(&slot));
            self.slot = Some(slot);

            // unlock the mutex by dropping the guard,
            // but remember the mutex so we can lock it later.
            let guard = self.guard.take().unwrap();
            self.mutex = Some(guard.mutex);
            drop(guard);

            return Poll::Pending;
        } else if let Some(ref slot) = self.slot {
            let mut inner = slot.lock().unwrap();
            if inner.is_some() {
                // spurious wake up -- nobody took our waker
                // we'll update this stale waker with a fresh one
                *inner = Some(cx.waker().clone());
                return Poll::Pending;
            }
        }
        // attempt to get the lock and return the guard
        let mutex = pin!(self.mutex.unwrap().lock());
        // lock may be contended in the event of a .notify_all()
        mutex.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    };

    fn make_executor() -> Arc<smol::Executor<'static>> {
        let ex = Arc::new(smol::Executor::new());
        for _ in 0..4 {
            let ex = Arc::clone(&ex);
            std::thread::spawn(move || smol::block_on(ex.run(std::future::pending::<()>())));
        }
        ex
    }

    // Tests that notify_one wakes exactly one waiter at a time.
    // Workers each consume one "token" (counter decrement).
    // Notifier adds one token and signals once per worker.
    #[test]
    fn condvar_notify_one() {
        let ex = make_executor();
        let cv = Arc::new(Condvar::new());
        let mutex = Arc::new(Mutex::new(0usize));
        let completed = Arc::new(AtomicUsize::new(0));
        let num_workers = 4;

        smol::block_on(async move {
            let workers: Vec<_> = (0..num_workers)
                .map(|_| {
                    let cv = Arc::clone(&cv);
                    let mutex = Arc::clone(&mutex);
                    let completed = Arc::clone(&completed);
                    ex.spawn(async move {
                        let mut guard = mutex.lock().await;
                        while *guard == 0 {
                            guard = cv.wait(guard).await;
                        }
                        *guard -= 1;
                        drop(guard);
                        completed.fetch_add(1, Relaxed);
                    })
                })
                .collect();

            for _ in 0..num_workers {
                let mut guard = mutex.lock().await;
                *guard += 1;
                drop(guard);
                cv.notify_one();
                smol::future::yield_now().await;
            }

            future::join_all(workers).await;
            assert_eq!(completed.load(Relaxed), num_workers);
            assert_eq!(*mutex.lock().await, 0); // all tokens consumed
        });
    }

    // Tests that notify_all wakes every waiter in one shot.
    #[test]
    fn condvar_notify_all() {
        let ex = make_executor();
        let cv = Arc::new(Condvar::new());
        let mutex = Arc::new(Mutex::new(false));
        let completed = Arc::new(AtomicUsize::new(0));
        let num_workers = 4;

        smol::block_on(async move {
            let workers: Vec<_> = (0..num_workers)
                .map(|_| {
                    let cv = Arc::clone(&cv);
                    let mutex = Arc::clone(&mutex);
                    let completed = Arc::clone(&completed);
                    ex.spawn(async move {
                        let mut guard = mutex.lock().await;
                        while !*guard {
                            guard = cv.wait(guard).await;
                        }
                        drop(guard);
                        completed.fetch_add(1, Relaxed);
                    })
                })
                .collect();

            smol::future::yield_now().await; // let workers reach cv.wait

            let mut guard = mutex.lock().await;
            *guard = true;
            drop(guard);
            cv.notify_all();

            future::join_all(workers).await;
            assert_eq!(completed.load(Relaxed), num_workers);
        });
    }
}
