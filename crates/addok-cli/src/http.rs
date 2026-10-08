//! The HTTP server: addok-csv's `/search/csv` over an index, `/batch`,
//! Parquet or CSV in and out (see `batch`), and `/health`. Each request runs its rows in
//! order on one thread, as many requests at once as the cores it is given:
//! requests of consecutive rows keep their caches warm.
//!
//! `/batch` takes the file as `data`, and as parameters: `columns`, once per
//! column a row's query joins (default: all); `input_format` and
//! `output_format`, `parquet` or `csv` (default: the file's extension, then
//! the input's); `input_delimiter` and `output_delimiter` (default `;`);
//! `min_score` (default 0.5).
//!
//! Both take `postcode_fallback` (`1`, `true`, `on`…; default off): a row
//! whose best result is not a confident house number is searched again
//! without its postcode where that can help. And both take
//! `result_columns`, once per column: `result_num`, `result_num_complement`
//! and `result_num_complement_short` are added only when named there;
//! other names change nothing, as addok-csv ignores them.
//!
//! Like addok, the server trusts its client: it takes uploads of any size
//! into memory, and `/search/csv` sniffs the whole file with Python's
//! Sniffer, whose patterns can take time quadratic in a crafted file's
//! size. Answering as addok-csv rules out a sample or a limit; exposed beyond a trusted
//! client, the server needs a front that bounds requests.

use std::net::SocketAddr;
use std::sync::Arc;

use addok_core::index::Index;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::Semaphore;

use crate::batch::{self, Format, Options};
use crate::geocoded::{FilterColumns, MIN_SCORE, Warning};
use crate::search_csv::{self, Error, Request, flag};

struct Server<B> {
    index: Index<B>,
    /// The cores geocoding runs on.
    cores: usize,
    /// One permit per core.
    permits: Arc<Semaphore>,
}

/// Serves the index on `address` until Ctrl-C, geocoding on `cores` threads
/// at most.
pub async fn serve<B>(index: Index<B>, address: SocketAddr, cores: usize) -> std::io::Result<()>
where
    B: AsRef<[u8]> + Send + Sync + 'static,
{
    let server = Arc::new(Server {
        index,
        cores,
        permits: Arc::new(Semaphore::new(cores)),
    });
    let app = Router::new()
        .route("/health", get(health::<B>))
        .route("/search/csv", post(search_csv::<B>))
        .route("/batch", post(batch::<B>))
        // addok sets no limit on the file.
        .layer(DefaultBodyLimit::disable())
        .with_state(server);
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("Serving HTTP on {address}, on {cores} cores…");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

/// Whether the server answers, and with which index: no geocoding, no
/// permit, so it answers at once even under load. `status` is what addok's
/// own `/health` says.
async fn health<B>(State(server): State<Arc<Server<B>>>) -> Response
where
    B: AsRef<[u8]> + Send + Sync + 'static,
{
    let body = serde_json::json!({
        "status": "HEALTHY",
        "version": env!("CARGO_PKG_VERSION"),
        "documents": server.index.len(),
        "cores": server.cores,
    });
    ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

async fn search_csv<B>(State(server): State<Arc<Server<B>>>, multipart: Multipart) -> Response
where
    B: AsRef<[u8]> + Send + Sync + 'static,
{
    let request = match form(multipart).await {
        Ok(request) => request,
        Err(title) => return error(StatusCode::BAD_REQUEST, &title),
    };
    let permit = server.permits.clone().acquire_owned().await.expect("never closed");
    let answer = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        search_csv::search_csv(&server.index, &request)
    });
    match answer.await {
        Ok(Ok(response)) => {
            let answered = (
                [
                    (header::CONTENT_TYPE, response.content_type),
                    (header::CONTENT_DISPOSITION, response.content_disposition),
                ],
                response.body,
            );
            warned(answered.into_response(), "/search/csv", &response.warnings)
        }
        Ok(Err(Error::BadRequest(title))) => error(StatusCode::BAD_REQUEST, &title),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
    }
}

async fn batch<B>(State(server): State<Arc<Server<B>>>, multipart: Multipart) -> Response
where
    B: AsRef<[u8]> + Send + Sync + 'static,
{
    let request = match form(multipart).await {
        Ok(request) => request,
        Err(title) => return error(StatusCode::BAD_REQUEST, &title),
    };
    let (input, output, options) = match batch_options(&request) {
        Ok(parsed) => parsed,
        Err(title) => return error(StatusCode::BAD_REQUEST, &title),
    };
    let permit = server.permits.clone().acquire_owned().await.expect("never closed");
    let filename = request.filename.clone();
    let answer = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let table = batch::read(request.data, input)?;
        let (geocoded, warnings) = batch::geocode_table(&server.index, &table, &options)?;
        Ok((batch::write(&geocoded, output)?, warnings))
    });
    match answer.await {
        Ok(Ok((body, warnings))) => {
            let stem = filename.rsplit_once('.').map_or(filename.as_str(), |(stem, _)| stem);
            let disposition = format!("attachment; filename=\"{stem}.geocoded.{}\"", output.extension());
            let answered = (
                [
                    (header::CONTENT_TYPE, output.content_type().to_owned()),
                    (header::CONTENT_DISPOSITION, disposition),
                ],
                body,
            );
            warned(answered.into_response(), "/batch", &warnings)
        }
        Ok(Err(batch::Error(title))) => error(StatusCode::BAD_REQUEST, &title),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
    }
}

/// The answer with an `X-Addok-Warning` header for each warning, each also
/// logged: rows the request left undone without failing it.
fn warned(mut response: Response, route: &str, warnings: &[Warning]) -> Response {
    for warning in warnings {
        eprintln!("{route}: warning: {warning}");
        let value = HeaderValue::from_str(&warning.header()).expect("ASCII");
        response.headers_mut().append(WARNING, value);
    }
    response
}

const WARNING: HeaderName = HeaderName::from_static("x-addok-warning");

/// `/batch`'s formats and options, from its parameters.
fn batch_options(request: &Request) -> Result<(Format, Format, Options), String> {
    let param = |name: &str| request.params.get(name).and_then(|values| values.last());
    let delimiter = |name: &str| match param(name) {
        None => Ok(batch::DELIMITER),
        Some(value) => {
            let mut chars = value.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(c),
                _ => Err(format!("\"{name}\" must be one character")),
            }
        }
    };
    let (input_delimiter, output_delimiter) = (delimiter("input_delimiter")?, delimiter("output_delimiter")?);
    let named = |name: &str, delimiter| {
        Format::named(name, delimiter).ok_or(format!("unknown format \"{name}\": parquet or csv"))
    };
    let input = match param("input_format") {
        Some(name) => named(name, input_delimiter)?,
        None => Format::of_file(&request.filename, input_delimiter)
            .ok_or("name the file's format with \"input_format\"")?,
    };
    let output = match param("output_format") {
        Some(name) => named(name, output_delimiter)?,
        None => match input {
            Format::Parquet => Format::Parquet,
            Format::Csv { .. } => Format::Csv { delimiter: output_delimiter },
        },
    };
    let min_score = match param("min_score") {
        None => MIN_SCORE,
        Some(value) => value.trim().parse().map_err(|_| format!("invalid \"min_score\": {value}"))?,
    };
    let postcode_fallback = match param("postcode_fallback") {
        None => false,
        Some(value) => flag(value).ok_or(format!("invalid \"postcode_fallback\": {value}"))?,
    };
    let columns = request.params.get("columns").cloned().unwrap_or_default();
    let result_columns = request.params.get("result_columns").cloned().unwrap_or_default();
    let filters = FilterColumns::named(|name| request.params.get(name).map(Vec::as_slice));
    let options = Options {
        columns,
        min_score,
        threads: 1,
        postcode_fallback,
        result_columns,
        filters,
    };
    Ok((input, output, options))
}

/// addok-csv's `parse_multipart`: the first `data` part is the file, every
/// other part a parameter. The query string is not read.
async fn form(mut multipart: Multipart) -> Result<Request, String> {
    let mut request = Request::default();
    let mut file = false;
    while let Some(field) = multipart.next_field().await.map_err(|e| e.body_text())? {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "data" && !file {
            request.filename = field.file_name().unwrap_or_default().to_owned();
            request.data = field.bytes().await.map_err(|e| e.body_text())?.to_vec();
            file = true;
        } else {
            let text = field.text().await.map_err(|e| e.body_text())?;
            request.params.entry(name).or_default().push(text);
        }
    }
    match file {
        true => Ok(request),
        false => Err("Missing file".into()),
    }
}

/// An error as Falcon writes one: JSON, its title escaped as JSON wants,
/// whatever the client sent.
fn error(status: StatusCode, title: &str) -> Response {
    let body = serde_json::json!({ "title": title }).to_string();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_errors_as_json() {
        let title = "Invalid parameter \"min_score\": \u{7}\u{200b}";
        let response = error(StatusCode::BAD_REQUEST, title);
        let body = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["title"], title);
    }

    #[test]
    fn answers_health_with_its_index() {
        let line = r#"{"id":"01002","banId":null,"type":"municipality","name":"L'Abergement-de-Varey","postcode":["01640"],"citycode":"01002","lon":5.420189,"lat":46.008573,"city":"L'Abergement-de-Varey","importance":0.22554}"#;
        let document = addok_core::document::Document::from_ndjson(line).unwrap();
        let mut bytes = addok_core::index::AlignedBytes::default();
        addok_core::index::write([document], &mut bytes).unwrap();
        let server = Arc::new(Server {
            index: Index::open(bytes).unwrap(),
            cores: 3,
            permits: Arc::new(Semaphore::new(3)),
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let response = runtime.block_on(health(State(server)));
        assert_eq!(response.status(), StatusCode::OK);
        let body = runtime
            .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "HEALTHY");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!((json["documents"].as_u64(), json["cores"].as_u64()), (Some(1), Some(3)));
    }

    fn request(filename: &str, params: &[(&str, &str)]) -> Request {
        let mut request = Request {
            filename: filename.into(),
            ..Request::default()
        };
        for &(name, value) in params {
            request.params.entry(name.into()).or_default().push(value.into());
        }
        request
    }

    #[test]
    fn reads_batch_options() {
        let (input, output, options) = batch_options(&request("addresses.parquet", &[])).unwrap();
        assert_eq!((input, output), (Format::Parquet, Format::Parquet));
        assert_eq!((options.min_score, options.threads), (MIN_SCORE, 1));
        assert!(!options.postcode_fallback);
        assert!(options.columns.is_empty() && options.result_columns.is_empty());
        // The output follows the input, with its own delimiter.
        let (input, output, _) =
            batch_options(&request("addresses.csv", &[("output_delimiter", ",")])).unwrap();
        assert_eq!(input, Format::Csv { delimiter: ';' });
        assert_eq!(output, Format::Csv { delimiter: ',' });
        let params = [
            ("input_format", "csv"),
            ("output_format", "parquet"),
            ("columns", "ad3"),
            ("columns", "city"),
            ("min_score", "0.7"),
            ("postcode_fallback", "1"),
            ("result_columns", "result_label"),
            ("result_columns", "result_num"),
        ];
        let (input, output, options) = batch_options(&request("chunk", &params)).unwrap();
        assert_eq!((input, output), (Format::Csv { delimiter: ';' }, Format::Parquet));
        assert_eq!(options.columns, ["ad3", "city"]);
        assert_eq!(options.min_score, 0.7);
        assert!(options.postcode_fallback);
        assert_eq!(options.result_columns, ["result_label", "result_num"]);
        // Nothing names the format; delimiters are one character.
        assert!(batch_options(&request("addresses", &[])).is_err());
        assert!(batch_options(&request("a.csv", &[("input_delimiter", ";;")])).is_err());
        assert!(batch_options(&request("a.csv", &[("min_score", "high")])).is_err());
        assert!(batch_options(&request("a.csv", &[("postcode_fallback", "maybe")])).is_err());
    }
}
