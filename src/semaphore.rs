use crate::{Mutex, Condvar};

pub struct Semaphore {
    mutex: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    pub fn new(permit_count: usize) -> Self {
        Self {
            mutex: Mutex::new(permit_count),
            cv: Condvar::new(),
        }
    }

    pub async fn acquire(&self) {
        let mut guard = self.mutex.lock().await;
        while *guard == 0 {
            guard = self.cv.wait(guard).await;
        }
        *guard -= 1;
    } 

    pub async fn release(&self) {
        let mut guard = self.mutex.lock().await;
        *guard += 1;
        self.cv.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    #[test]
    fn test_semaphore() {
        let max_permits = 3;
        let num_tasks = 12;

        let sem = Arc::new(Semaphore::new(max_permits));
        let peak = Arc::new(AtomicUsize::new(0));
        let inside = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        let ex = Arc::new(smol::Executor::new());
        let _threads: Vec<_> = (0..4)
            .map(|_| {
                let ex = Arc::clone(&ex);
                std::thread::spawn(move || smol::block_on(ex.run(std::future::pending::<()>())))
            })
            .collect();

        smol::block_on(ex.run(async {
            let tasks: Vec<_> = (0..num_tasks)
                .map(|id| {
                    let sem = Arc::clone(&sem);
                    let peak = Arc::clone(&peak);
                    let inside = Arc::clone(&inside);
                    let completed = Arc::clone(&completed);
                    ex.spawn(async move {
                        sem.acquire().await;

                        let current = inside.fetch_add(1, SeqCst) + 1;
                        peak.fetch_max(current, SeqCst);
                        assert!(current <= max_permits, "too many inside: {current}");
                        println!("[{id}] acquired permit, concurrency now: {current}");

                        smol::future::yield_now().await; // hold the permit across a yield

                        inside.fetch_sub(1, SeqCst);
                        sem.release().await;
                        let done = completed.fetch_add(1, SeqCst) + 1;
                        println!("[{id}] released permit, tasks completed: {done}");
                    })
                })
                .collect();

            futures::future::join_all(tasks).await;

            assert_eq!(completed.load(SeqCst), num_tasks);
            assert!(peak.load(SeqCst) > 1, "peak concurrency should exceed 1");
            assert!(peak.load(SeqCst) <= max_permits);
            println!("peak concurrency: {}/{max_permits}", peak.load(SeqCst));
        }));
    }
}