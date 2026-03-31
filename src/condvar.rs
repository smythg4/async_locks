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
    use std::sync::{Arc, atomic::AtomicUsize};
    #[test]
    fn condvar_basics() {
        let cv = Arc::new(Condvar::new());
        let mutex = Arc::new(Mutex::new(0usize));
        let ex = Arc::new(smol::Executor::new());
        let counter= Arc::new(AtomicUsize::new(0));

        // spawn N OS threads all running the same executor
        let _threads: Vec<_> = (0..8)
            .map(|_| {
                let ex = Arc::clone(&ex);
                std::thread::spawn(move || smol::block_on(ex.run(std::future::pending::<()>())))
            })
            .collect();

        smol::block_on(async move {
            let workers: Vec<_> = (0..4)
                .map(|id| {
                    let cv = Arc::clone(&cv);
                    let mutex = Arc::clone(&mutex);
                    let counter = Arc::clone(&counter);
                    async move {
                        for i in 1..3 {
                            let guard = mutex.lock().await;
                            println!("[{id}] going to sleep ({i})...");
                            cv.wait(guard).await;
                            println!("[{id}] waking up ({i})...");
                            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        println!("[{id}] All done!");
                    }
                })
                .map(|f| ex.spawn(f))
                .collect();

            let wake_ups = ex.spawn(async move {
                for i in 0..6 {
                    if i % 2 == 0 {
                        println!("Wake one up!");
                        cv.notify_one();
                    } else {
                        println!("Wake 'em all up!");
                        cv.notify_all();
                    }
                    smol::Timer::after(std::time::Duration::from_secs(1)).await;
                }
            });

            ex.run(async {
                let all_tasks: Vec<_> = workers.into_iter().chain([wake_ups]).collect();
                future::join_all(all_tasks).await;
                let count = counter.load(std::sync::atomic::Ordering::Relaxed);
                assert_eq!(count, 4*2);
            })
            .await;
        })
    }
}
