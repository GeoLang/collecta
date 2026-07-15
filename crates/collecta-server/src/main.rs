//! Collecta REST API server binary.

#[tokio::main]
async fn main() {
    let app = collecta_server::app().await;
    let addr = std::env::var("COLLECTA_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind listener");
    println!("collecta-server listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
