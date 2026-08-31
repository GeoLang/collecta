//! Requests to the server: the client, the timeout, the bearer token and the
//! way a rejection turns into a message, in one place for every command.

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn post<Response: DeserializeOwned>(
    server: &str,
    path: &str,
    token: Option<&str>,
    body: &impl Serialize,
) -> std::result::Result<Response, String> {
    let client = client()?;
    send(client.post(url(server, path)).json(body), token)
}

/// `query` is percent-encoded, which the form cursor needs: it carries a `+`
/// whenever the server's timestamp has a positive offset.
pub fn get<Response: DeserializeOwned>(
    server: &str,
    path: &str,
    token: Option<&str>,
    query: &[(&str, &str)],
) -> std::result::Result<Response, String> {
    let client = client()?;
    send(client.get(url(server, path)).query(query), token)
}

fn url(server: &str, path: &str) -> String {
    format!("{}{path}", server.trim_end_matches('/'))
}

fn client() -> std::result::Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())
}

fn send<Response: DeserializeOwned>(
    mut builder: reqwest::blocking::RequestBuilder,
    token: Option<&str>,
) -> std::result::Result<Response, String> {
    if let Some(token) = token {
        builder = builder.bearer_auth(token);
    }
    let response = builder.send().map_err(|error| error.to_string())?;

    let status = response.status();
    if status.is_success() {
        return response.json().map_err(|error| error.to_string());
    }
    let body = response.text().unwrap_or_default();
    let detail = body.trim();
    if detail.is_empty() {
        return Err(format!("server returned {status}"));
    }
    Err(format!("server returned {status}: {detail}"))
}
