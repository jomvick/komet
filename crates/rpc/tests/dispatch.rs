//! `serve_connection` dispatch: duplicate ids and abort-on-close.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use komet_rpc::{RpcError, RpcReply, RpcService, serve_connection};
use tokio::sync::mpsc;

struct Slow(Arc<AtomicBool>);

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl RpcService for Slow {
    async fn handle(&self, method: &str, _p: serde_json::Value) -> Result<RpcReply, RpcError> {
        let _guard = DropFlag(self.0.clone());
        if method == "Slow" {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(RpcReply::Value(serde_json::json!(method)))
    }
}

fn setup(dropped: Arc<AtomicBool>) -> (mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (in_tx, in_rx) = mpsc::channel(8);
    let (out_tx, out_rx) = mpsc::channel(8);
    tokio::spawn(serve_connection(Arc::new(Slow(dropped)), out_tx, in_rx));
    (in_tx, out_rx)
}

#[tokio::test]
async fn duplicate_in_flight_id_is_dropped_and_original_replies_once() {
    let (tx, mut rx) = setup(Arc::default());
    tx.send(r#"{"id":1,"method":"Slow","params":null}"#.into())
        .await
        .unwrap();
    tx.send(r#"{"id":1,"method":"Fast","params":null}"#.into())
        .await
        .unwrap();
    let first = rx.recv().await.unwrap();
    assert!(first.contains("Slow"), "{first}");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn tasks_aborted_when_inbound_closes() {
    let dropped = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = setup(dropped.clone());
    tx.send(r#"{"id":2,"method":"Slow","params":null}"#.into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(tx);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(dropped.load(Ordering::SeqCst));
    assert!(rx.recv().await.is_none());
}
