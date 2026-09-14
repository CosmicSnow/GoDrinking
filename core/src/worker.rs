//! Bounded serial work on one OS thread. Codec objects are created and
//! destroyed there, so a suspended async task never migrates their OS state.
use tokio::sync::{mpsc, oneshot};

pub(crate) struct SerialWorker<I, O> {
    tx: mpsc::Sender<(I, oneshot::Sender<O>)>,
}

impl<I: Send + 'static, O: Send + 'static> SerialWorker<I, O> {
    pub(crate) fn start<F: FnMut(I) -> O + 'static>(
        name: &str,
        initialize: impl FnOnce() -> F + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, mut rx) = mpsc::channel::<(I, oneshot::Sender<O>)>(1);
        std::thread::Builder::new().name(name.into()).spawn(move || {
            let mut work = initialize();
            while let Some((input, reply)) = rx.blocking_recv() {
                // A cancelled caller need not start additional work. An
                // already running codec finishes before destruction here.
                if !reply.is_closed() { let _ = reply.send(work(input)); }
            }
        })?;
        Ok(Self { tx })
    }

    // Exclusive access guarantees one request in flight, including its reply.
    pub(crate) async fn run(&mut self, input: I) -> Result<O, &'static str> {
        let (reply, result) = oneshot::channel();
        self.tx.send((input, reply)).await.map_err(|_| "codec worker closed")?;
        result.await.map_err(|_| "codec worker failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_codec_does_not_block_async_network_progress() {
        let (release, wait) = std::sync::mpsc::channel();
        let mut worker = SerialWorker::start("test-codec", move || move |()| {
            wait.recv_timeout(Duration::from_secs(2)).expect("async runtime must remain free to release the codec")
        }).unwrap();
        let (decoded, ()) = tokio::join!(worker.run(()), async {
            tokio::task::yield_now().await;
            release.send(42).unwrap();
        });
        assert_eq!(decoded.unwrap(), 42);
    }

    #[tokio::test]
    async fn codec_keeps_order_thread_affinity_and_drops_after_close() {
        struct Guard(std::sync::mpsc::Sender<()>);
        impl Drop for Guard { fn drop(&mut self) { let _ = self.0.send(()); } }
        let (dropped, wait) = std::sync::mpsc::channel();
        let caller = std::thread::current().id();
        let mut worker = SerialWorker::start("test-serial", move || {
            let guard = Guard(dropped);
            let owner = std::thread::current().id();
            let mut count = 0;
            move |expected| {
                let _ = &guard;
                assert_eq!(std::thread::current().id(), owner);
                count += 1;
                assert_eq!(count, expected);
                owner
            }
        }).unwrap();
        for expected in 1..=12 { assert_ne!(worker.run(expected).await.unwrap(), caller); }
        drop(worker);
        tokio::task::spawn_blocking(move || wait.recv_timeout(Duration::from_secs(2)).unwrap()).await.unwrap();
    }
}
