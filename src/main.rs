mod porkbun;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use axum_client_ip::InsecureClientIp;
use chrono::Local;
use clap::Parser as CliParser;
use clap_verbosity_flag::{InfoLevel, Verbosity};
use env_logger::Builder;
use log::{error, info};
use nanoid::nanoid;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::net::SocketAddr;

use crate::porkbun::Porkbun;

#[derive(CliParser, Debug, Clone)]
#[command(version, about, long_about = None)]
struct CLI {
    /// Host
    #[arg(long, default_value = "0.0.0.0:3000")]
    host: String,

    /// Porkbun API key
    #[arg(long)]
    porkbun_api_key: String,

    /// Porkbun secret key
    #[arg(long)]
    porkbun_secret_key: String,

    /// Domain
    #[arg(long)]
    domain: String,

    /// Verbosity level
    #[clap(flatten)]
    verbose: Verbosity<InfoLevel>,

    /// Authentication token
    #[arg(long)]
    token: Option<String>,
}

fn set_logging(cli: &CLI) {
    Builder::new()
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] - {}",
                Local::now().format("%Y-%m-%dT%H:%M:%S"),
                record.level(),
                record.args()
            )
        })
        .filter_module("dyndns", cli.verbose.log_level_filter())
        .init();
}

#[derive(Deserialize)]
struct Params {
    token: String,
    subdomain: Option<String>,
    a: Option<String>,
    aaaa: Option<String>,
    txt: Option<String>,
    clear: Option<bool>,
}

use axum::response::Response as AxumResponse;

#[derive(Deserialize, Serialize)]
struct RecordResponse {
    r#type: String,
    content: String,
}

#[derive(Deserialize, Serialize)]
struct Response {
    message: String,
    domain: String,
    clear: bool,
    records: Vec<RecordResponse>,
}

impl IntoResponse for Response {
    fn into_response(self) -> AxumResponse {
        AxumResponse::new(axum::body::Body::empty())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = CLI::parse();
    set_logging(&cli);

    let app = Router::new().route("/", get(root)).with_state(cli.clone());

    let listener = tokio::net::TcpListener::bind(cli.host).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

fn response(
    status: StatusCode,
    message: &str,
    domain: &str,
    records: Vec<(String, String)>,
    clear: bool,
) -> (StatusCode, Json<Response>) {
    let mut record_responses = vec![];
    for (record_type, record_content) in records {
        record_responses.push(RecordResponse {
            r#type: record_type,
            content: record_content,
        });
    }

    (
        status,
        Json(Response {
            message: String::from(message),
            domain: String::from(domain),
            records: record_responses,
            clear: clear,
        }),
    )
}

fn extract_subdomain(domain: String) -> (String, String) {
    let parts: Vec<&str> = domain.split('.').collect();
    let subdomain = parts[0..parts.len() - 2].join(".");
    let domain = parts[parts.len() - 2..].join(".");
    (subdomain, domain)
}

async fn handle_record(
    porkbun: &Porkbun,
    subdomain: String,
    record_type: String,
    content: String,
    clear: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    porkbun
        .delete_record(&subdomain, &record_type, &content)
        .await?;

    if clear {
        return Ok(());
    }

    porkbun
        .create_record(&subdomain, &record_type, &content)
        .await?;

    Ok(())
}

#[axum::debug_handler]
async fn root(
    State(cli): State<CLI>,
    params: Query<Params>,
    InsecureClientIp(client_ip): InsecureClientIp,
) -> impl IntoResponse {
    let is_clear = params.clear.unwrap_or(false);
    if let Some(token) = &cli.token {
        if token != &params.token {
            return response(
                StatusCode::UNAUTHORIZED,
                "Unauthorized: Invalid token",
                "",
                vec![],
                is_clear,
            );
        }
    }

    // Extract subdomain and domain
    let (subdomain, domain) = extract_subdomain(cli.domain.clone());

    // Generate a random subdomain if not provided
    let user_subdomain = match params.subdomain.clone() {
        Some(subdomain) => subdomain,
        None => nanoid!(7, &"1234567890abcdef".chars().collect::<Vec<char>>()),
    };

    // Construct final subdomain and domain
    let full_subdomain = if subdomain.is_empty() {
        user_subdomain
    } else {
        format!("{}.{}", user_subdomain, subdomain)
    };
    let full_domain = format!("{}.{}", full_subdomain, domain);

    // Get which records to update (A, AAAA, TXT)
    let mut records = vec![];
    if let Some(content) = params.a.clone() {
        records.push((String::from("A"), content));
    }
    if let Some(content) = params.aaaa.clone() {
        records.push((String::from("AAAA"), content));
    }
    if let Some(content) = params.txt.clone() {
        records.push((String::from("TXT"), content));
    }

    // If no A or AAAA record is provided, use the client's IP address
    // This is working even behind a reverse proxy
    if params.a.is_none() && params.aaaa.is_none() {
        if client_ip.is_ipv4() {
            records.push((String::from("A"), client_ip.to_string()))
        } else {
            records.push((String::from("AAAA"), client_ip.to_string()))
        }
    }

    let porkbun = Porkbun::new(cli.porkbun_api_key, cli.porkbun_secret_key, domain.clone());
    for (record_type, content) in records.clone().into_iter() {
        match handle_record(
            &porkbun,
            full_subdomain.clone(),
            record_type.clone(),
            content.clone(),
            is_clear,
        )
        .await
        {
            Ok(_) => {
                let mut action = "updated";
                if is_clear {
                    action = "deleted"
                }

                info!(
                    "Record {}: {} {} {}",
                    action, record_type, full_domain, content
                );
            }
            Err(e) => {
                error!(
                    "Error handling record: {} {} {}: {}",
                    record_type, full_domain, content, e
                );
                return response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Server Error",
                    &full_domain,
                    records.clone(),
                    is_clear,
                );
            }
        }
    }

    response(
        StatusCode::OK,
        "OK",
        &full_domain,
        records.clone(),
        is_clear,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_subdomain() {
        let (subdomain, domain) = extract_subdomain("subdomain.domain.com".to_string());
        assert_eq!(subdomain, "subdomain");
        assert_eq!(domain, "domain.com");

        let (subdomain, domain) = extract_subdomain("subdomain.other.co.uk".to_string());
        assert_eq!(subdomain, "subdomain.other");
        assert_eq!(domain, "co.uk");

        let (subdomain, domain) = extract_subdomain("domain.com".to_string());
        assert_eq!(subdomain, "");
        assert_eq!(domain, "domain.com");
    }
}
