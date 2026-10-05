use reqwest;
use tokio::sync::mpsc;
pub trait Notification {
    pub fn forward_notification(&self, mut records: mpsc::UnboundedReceiver<Value>) -> Self;
    //pub fn
}

pub struct Ntfy {
    ntfy_url: String,
    ntfy_topic: String,
}

impl Notification for Ntfy {
    fn forward_notification(&self, mut records: mpsc::UnboundedReceiver<Value>) {
        let render = self.ev_tx.clone();
        let ctl = self.ctl.clone();
        tokio::spawn(async move {
            while let Some(v) = records.recv().await {
                let client = reqwest::Client::new();

                // Simple JSON POST
                let response = client
                    .post(format!("{}/{}", self.ntfy_url, self.ntfy_topic))
                    /* .json(&json!({
                        "key": "value",
                        "name": "Rust"
                    }))*/
                    .send()
                    .await;
                match response {
                    Ok(r) => tracing::debug!("Ntfy response succeeded"),
                    Err(e) => tracing::warn!("Ntfy response failed with {}", e),
                }
            }
        });
    }
}
