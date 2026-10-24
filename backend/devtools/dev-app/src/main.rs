//! Creates a personal GitHub App for local development via GitHub's App manifest flow.
//!
//! 1. Serves a page on localhost that posts `deploy/manifests/dev.json` to GitHub.
//! 2. You approve the App on github.com; GitHub redirects back here with a one-time code.
//! 3. The code is exchanged for the App's credentials, which are written to `.secrets/` and
//!    `.env`. Nothing is sent anywhere except GitHub.

mod env_file;

use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, bail};
use axum::{
    Router,
    extract::{Query, State},
    response::{Html, IntoResponse},
    routing::get,
};
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::oneshot;

#[derive(Debug, Parser)]
#[command(about = "Create a personal MergeTip GitHub App for local development")]
struct Args {
    /// Suffix for the App name: `mergetip-dev-<name>`.
    #[arg(long, env = "USER")]
    name: String,
    /// Create the App under this organization instead of your account.
    #[arg(long)]
    org: Option<String>,
    /// Webhook proxy URL (https://smee.io/<channel>) that forwards deliveries to the local api.
    #[arg(long, env = "SMEE_URL")]
    smee_url: String,
    #[arg(long, default_value = "../deploy/manifests/dev.json")]
    manifest: PathBuf,
    #[arg(long, default_value = "../.env")]
    env_file: PathBuf,
    #[arg(long, default_value = "../.secrets")]
    secrets_dir: PathBuf,
    #[arg(long, default_value_t = 3456)]
    port: u16,
}

/// What GitHub returns from `POST /app-manifests/{code}/conversions`.
#[derive(Debug, Deserialize)]
struct Conversion {
    id: u64,
    slug: String,
    html_url: String,
    pem: String,
    webhook_secret: Option<String>,
    client_id: String,
    client_secret: String,
}

struct AppState {
    form_page: String,
    state_token: String,
    args: Args,
    done: Mutex<Option<oneshot::Sender<()>>>,
}

#[derive(Debug, Deserialize)]
struct Callback {
    code: String,
    state: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let args = Args::parse();
    if !args.smee_url.starts_with("https://") {
        bail!("--smee-url must be an https URL; create a channel at https://smee.io/new");
    }

    let manifest = build_manifest(&args)?;
    let state_token = mergetip_crypto::random_token()?.raw.expose().clone();
    let target = match &args.org {
        Some(org) => format!("https://github.com/organizations/{org}/settings/apps/new"),
        None => "https://github.com/settings/apps/new".to_owned(),
    };
    let form_page = form_page(&target, &state_token, &manifest);

    let (done_tx, done_rx) = oneshot::channel();
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let state = Arc::new(AppState {
        form_page,
        state_token,
        args,
        done: Mutex::new(Some(done_tx)),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/callback", get(callback))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("open http://{addr}/ in your browser to create the App");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            done_rx.await.ok();
        })
        .await?;
    Ok(())
}

/// The dev manifest with this developer's name, webhook proxy and local redirect filled in.
fn build_manifest(args: &Args) -> anyhow::Result<Value> {
    let raw = fs::read_to_string(&args.manifest)
        .with_context(|| format!("cannot read {}", args.manifest.display()))?;
    let mut manifest: Value = serde_json::from_str(&raw).context("manifest is not JSON")?;
    let name = manifest["name"]
        .as_str()
        .unwrap_or("mergetip-dev-<name>")
        .replace("<name>", &args.name);
    manifest["name"] = Value::String(name);
    manifest["hook_attributes"]["url"] = Value::String(args.smee_url.clone());
    manifest["redirect_url"] = Value::String(format!("http://localhost:{}/callback", args.port));
    Ok(manifest)
}

fn form_page(target: &str, state: &str, manifest: &Value) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Create MergeTip dev App</title></head>
<body style="font-family: system-ui; max-width: 40rem; margin: 4rem auto">
<h1>Create your MergeTip dev App</h1>
<p>GitHub will ask you to confirm the App's name and permissions.</p>
<form action="{target}?state={state}" method="post">
<input type="hidden" name="manifest" value="{manifest}">
<button type="submit">Create GitHub App</button>
</form>
</body></html>"#,
        target = html_escape(target),
        state = html_escape(state),
        manifest = html_escape(&manifest.to_string()),
    )
}

async fn index(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(state.form_page.clone())
}

async fn callback(
    State(state): State<Arc<AppState>>,
    Query(query): Query<Callback>,
) -> impl IntoResponse {
    if query.state != state.state_token {
        return Html("<p>State mismatch. Start again from the first page.</p>".to_owned());
    }
    let page = match finish(&state.args, &query.code).await {
        Ok(conversion) => format!(
            "<p>Created <a href=\"{url}\">{slug}</a>. Credentials are in <code>.env</code> and \
             <code>.secrets/</code>.</p><p>Next: <a href=\"{url}/installations/new\">install it on a \
             test repository</a>, then run <code>make smee</code> and <code>make dev</code>. You \
             can close this tab.</p>",
            url = html_escape(&conversion.html_url),
            slug = html_escape(&conversion.slug),
        ),
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "creating the App failed");
            format!(
                "<p>Creating the App failed: {}</p>",
                html_escape(&format!("{err:#}"))
            )
        }
    };
    if let Some(done) = state.done.lock().ok().and_then(|mut d| d.take()) {
        done.send(()).ok();
    }
    Html(page)
}

async fn finish(args: &Args, code: &str) -> anyhow::Result<Conversion> {
    if !code.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("unexpected code format");
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("mergetip-dev-app/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?;
    let conversion: Conversion = client
        .post(format!(
            "https://api.github.com/app-manifests/{code}/conversions"
        ))
        .header("accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let key_path = args.secrets_dir.join("github-app.pem");
    write_private(&args.secrets_dir, &key_path, conversion.pem.as_bytes())?;

    let env = fs::read_to_string(&args.env_file).unwrap_or_default();
    let app_id = conversion.id.to_string();
    let key_path_value = key_path.to_string_lossy().into_owned();
    let mut updates = vec![
        ("GITHUB_APP_ID", app_id.as_str()),
        ("GITHUB_APP_SLUG", conversion.slug.as_str()),
        ("GITHUB_APP_PRIVATE_KEY_PATH", key_path_value.as_str()),
        ("GITHUB_CLIENT_ID", conversion.client_id.as_str()),
        ("GITHUB_CLIENT_SECRET", conversion.client_secret.as_str()),
        ("SMEE_URL", args.smee_url.as_str()),
    ];
    if let Some(secret) = conversion.webhook_secret.as_deref() {
        updates.push(("GITHUB_WEBHOOK_SECRET", secret));
    }
    fs::write(&args.env_file, env_file::upsert(&env, &updates))
        .with_context(|| format!("cannot write {}", args.env_file.display()))?;
    tracing::info!(app = %conversion.slug, "App created; .env updated");
    Ok(conversion)
}

/// Writes the private key readable by the owner only.
fn write_private(dir: &Path, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|mut file| file.write_all(contents))
        .with_context(|| format!("cannot write {}", path.display()))
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_gets_name_webhook_and_redirect() {
        let dir = std::env::temp_dir().join(format!("dev-app-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("dev.json");
        fs::write(
            &manifest_path,
            r#"{"name":"mergetip-dev-<name>","hook_attributes":{"url":"x"},"redirect_url":"x"}"#,
        )
        .unwrap();
        let args = Args {
            name: "ada".into(),
            org: None,
            smee_url: "https://smee.io/abc".into(),
            manifest: manifest_path,
            env_file: dir.join(".env"),
            secrets_dir: dir.join(".secrets"),
            port: 4000,
        };
        let manifest = build_manifest(&args).unwrap();
        assert_eq!(manifest["name"], "mergetip-dev-ada");
        assert_eq!(manifest["hook_attributes"]["url"], "https://smee.io/abc");
        assert_eq!(manifest["redirect_url"], "http://localhost:4000/callback");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn form_page_escapes_the_manifest() {
        let page = form_page(
            "https://github.com/settings/apps/new",
            "st",
            &serde_json::json!({ "name": "a\"b<c>" }),
        );
        assert!(page.contains("&quot;name&quot;:&quot;a\\&quot;b&lt;c&gt;&quot;"));
        assert!(!page.contains("<c>"));
    }

    #[cfg(unix)]
    #[test]
    fn private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dev-app-key-{}", std::process::id()));
        let path = dir.join("github-app.pem");
        write_private(&dir, &path, b"-----BEGIN KEY-----").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(dir).ok();
    }
}
