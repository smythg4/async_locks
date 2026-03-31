use crossbeam_queue::SegQueue;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::{Acquire, Release};
use std::task::{Poll, Waker};

use crate::mutex::{Mutex, MutexGuard};

pub struct Condvar {
    counter: AtomicU32,
    wakers: SegQueue<Waker>,
}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
            wakers: SegQueue::new(),
        }
    }

    pub fn notify_one(&self) {
        if let Some(w) = self.wakers.pop() {
            self.counter.fetch_add(1, Release);
            w.wake();
        }
    }

    pub fn notify_all(&self) {
        while let Some(w) = self.wakers.pop() {
            self.counter.fetch_add(1, Release);
            w.wake();
        }
    }

    pub fn wait<'a, 'b, T>(&'a self, guard: MutexGuard<'b, T>) -> CondvarWaitFuture<'a, 'b, T> {
        CondvarWaitFuture {
            guard: Some(guard),
            cv: self,
            mutex: None,
            counter_store: None,
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
    counter_store: Option<u32>,
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

        if self.counter_store.is_none() {
            // first time calling wait, register our waker, set an initial counter state and return pending
            self.counter_store = Some(self.cv.counter.load(Acquire));
            self.cv.wakers.push(cx.waker().clone());

            // unlock the mutex by dropping the guard,
            // but remember the mutex so we can lock it later.
            let guard = self.guard.take().unwrap();
            self.mutex = Some(guard.mutex);
            drop(guard);

            Poll::Pending
        } else if Some(self.cv.counter.load(Acquire)) == self.counter_store {
            // subsequent check, ensure that the counter value has changed, if not, register waker and return pending
            self.cv.wakers.push(cx.waker().clone());
            Poll::Pending
        } else {
            // counter changed! attempt to get the lock and return the guard
            let mutex = pin!(self.mutex.unwrap().lock());
            // should acquire the lock immediately here
            mutex.poll(cx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use std::sync::Arc;
    #[test]
    fn condvar_basics() {
        let cv = Arc::new(Condvar::new());
        let mutex = Arc::new(Mutex::new(0usize));

        smol::block_on(async move {
            let ex = smol::Executor::new();

            let workers: Vec<_> = (0..4)
                .map(|id| {
                    let cv = Arc::clone(&cv);
                    let mutex = Arc::clone(&mutex);
                    async move {
                        for i in 1..3 {
                            let guard = mutex.lock().await;
                            println!("[{id}] going to sleep ({i})...");
                            cv.wait(guard).await;
                            println!("[{id}] waking up ({i})...");
                        }
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
            })
            .await;
        })
    }
}
