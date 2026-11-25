use std::future::Future;
use tokio::sync::Mutex;

// Concurrent `npx -y ccusage@latest` runs share one npx cache entry. Two of
// them extracting it at once make a spawn fail with ETXTBSY (the file is still
// open for writing), so every npx spawn in the process goes through this lock.
static NPX_LOCK: Mutex<()> = Mutex::const_new(());

/// Run `fut` while holding the process-wide npx lock.
pub async fn run_exclusive<T>(fut: impl Future<Output = T>) -> T {
    let _guard = NPX_LOCK.lock().await;
    fut.await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn spawns_never_overlap() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (active, peak) = (active.clone(), peak.clone());
                tokio::spawn(async move {
                    run_exclusive(async {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await;
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "npx runs overlapped");
    }
}
