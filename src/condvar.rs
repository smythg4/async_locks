use std::future::Future;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::Mutex as SyncMutex;
use std::task::Poll;

use crate::mutex::{LockFuture, Mutex, MutexGuard};
use crate::waiter::Waiter;
use cordyceps::List;

#[derive(Default)]
pub struct Condvar {
    waiters: SyncMutex<List<Waiter>>,
}

impl Condvar {
    pub fn new() -> Self {
        Self {
            waiters: SyncMutex::new(List::new()),
        }
    }

    pub fn notify_one(&self) {
        let mut guard = self.waiters.lock().unwrap();
        if let Some(ptr) = guard.pop_front() {
            let w = unsafe { (*ptr.as_ptr()).waker.take().unwrap() };
            drop(guard);
            w.wake();
        }
    }

    pub fn notify_all(&self) {
        let mut guard = self.waiters.lock().unwrap();
        let mut wakers = Vec::new();
        while let Some(ptr) = guard.pop_front() {
            let w = unsafe { (*ptr.as_ptr()).waker.take().unwrap() };
            wakers.push(w);
        }
        drop(guard);
        wakers.into_iter().for_each(|w| w.wake());
    }

    pub fn wait<'a, 'b, T>(&'a self, guard: MutexGuard<'b, T>) -> CondvarWaitFuture<'a, 'b, T> {
        CondvarWaitFuture {
            cv: self,
            phase: CondvarPhase::Waiting(guard),
            waiter: Waiter::default(),
        }
    }
}

enum CondvarPhase<'b, T> {
    Waiting(MutexGuard<'b, T>),   // pre-first-poll, holding the guard
    Parked(&'b Mutex<T>),         // registered with condvar, waiting for notify
    Acquiring(Pin<Box<LockFuture<'b, T>>>), // notified, re-acquiring the mutex
    Sentinel,
}

pub struct CondvarWaitFuture<'a, 'b, T> {
    cv: &'a Condvar,
    phase: CondvarPhase<'b, T>,
    waiter: Waiter,
}

impl<'a, 'b, T> Drop for CondvarWaitFuture<'a, 'b, T> {
    fn drop(&mut self) {
        // this process is needed to eliminate stale wakers in the Condvar waker queue
         // essential for cancel safety
        let mut guard = self.cv.waiters.lock().unwrap();
        if self.waiter.waker.is_some() {
            let _ = unsafe { guard.remove(NonNull::from_ref(&self.waiter)) };
        }
    }
}

impl<'a, 'b, T> Future for CondvarWaitFuture<'a, 'b, T> {
    type Output = MutexGuard<'b, T>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match std::mem::replace(&mut this.phase, CondvarPhase::Sentinel) {
            CondvarPhase::Waiting(guard) => {
                let mutex_ref = guard.mutex;
                let mut waiter_guard = this.cv.waiters.lock().unwrap();
                this.waiter.add_waker(cx.waker().clone());
                waiter_guard.push_back(NonNull::from_ref(&this.waiter));

                drop(guard);
                drop(waiter_guard);
                this.phase = CondvarPhase::Parked(mutex_ref);
                Poll::Pending
            }
            CondvarPhase::Parked(mutex) => {
                let waiter_guard = this.cv.waiters.lock().unwrap();
                if this.waiter.waker.is_some() {
                    // we have a waker, lets just update it
                    this.waiter.add_waker(cx.waker().clone());
                    this.phase = CondvarPhase::Parked(mutex);
                    Poll::Pending
                } else {
                    // we were notified and waker was taken
                    drop(waiter_guard);
                    let mut lock_future = Box::pin(mutex.lock());
                    let result = lock_future.as_mut().poll(cx);
                    match result {
                        Poll::Pending => {
                            this.phase = CondvarPhase::Acquiring(lock_future);
                            Poll::Pending
                        }
                        _ => result,
                    }
                }
            }
            CondvarPhase::Acquiring(mut lock_future) => {
                let result = lock_future.as_mut().poll(cx);
                if result.is_pending() {
                    this.phase = CondvarPhase::Acquiring(lock_future);
                }
                result
            }
            CondvarPhase::Sentinel => unreachable!(),
        }
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
                .map(|id| {
                    let cv = Arc::clone(&cv);
                    let mutex = Arc::clone(&mutex);
                    let completed = Arc::clone(&completed);
                    ex.spawn(async move {
                        let mut guard = mutex.lock().await;
                        while *guard == 0 {
                            println!("[{id}] going to sleep");
                            guard = cv.wait(guard).await;
                        }
                        println!("[{id}] woken up!");
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
                println!("Waking one worker up!");
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
                .map(|id| {
                    let cv = Arc::clone(&cv);
                    let mutex = Arc::clone(&mutex);
                    let completed = Arc::clone(&completed);
                    ex.spawn(async move {
                        let mut guard = mutex.lock().await;
                        while *guard == false {
                            println!("[{id}] going to sleep");
                            guard = cv.wait(guard).await;
                        }
                        println!("[{id}] woken up!");
                        drop(guard);
                        completed.fetch_add(1, Relaxed);
                    })
                })
                .collect();

            smol::future::yield_now().await; // let workers reach cv.wait

            let mut guard = mutex.lock().await;
            *guard = true;
            drop(guard);
            println!("Waking everyone up!");
            cv.notify_all();

            future::join_all(workers).await;
            assert_eq!(completed.load(Relaxed), num_workers);
        });
    }
}
