//! Collecta REST API server binary.
//!
//! `collecta-server` serves the API; `collecta-server create-user <email>
//! [role]` seeds a user, reading the password from stdin (no signup endpoint).

use std::io::Read;

use collecta_server::store::UserRecord;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("create-user") {
        create_user(&args[2..]).await;
        return;
    }

    let app = collecta_server::app().await;
    let addr = std::env::var("COLLECTA_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind listener");
    println!("collecta-server listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}

async fn create_user(args: &[String]) {
    let Some(email) = args.first() else {
        eprintln!("usage: collecta-server create-user <email> [role] (password on stdin)");
        std::process::exit(2);
    };
    let role = args.get(1).cloned().unwrap_or_else(|| "admin".to_string());

    let mut password = String::new();
    std::io::stdin()
        .read_to_string(&mut password)
        .expect("failed to read password from stdin");
    let password = password.trim_end_matches(['\r', '\n']);
    if password.len() < 8 {
        eprintln!("password must be at least 8 characters");
        std::process::exit(2);
    }

    let store = collecta_server::open_store().await;
    let user = UserRecord {
        id: uuid::Uuid::new_v4(),
        email: email.clone(),
        password_hash: collecta_server::auth::hash_password(password),
        role,
    };
    store
        .create_user(&user)
        .await
        .expect("failed to create user (email already taken?)");
    println!("created user {email} ({})", user.id);
}
