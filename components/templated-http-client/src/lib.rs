//! Sends a request to a URI template expanded with typed values, and reads
//! the response stream from the componentized http client into a string.

mod expand;

wit_bindgen::generate!({
    path: "wit",
    world: "templated-http-client",
    generate_all,
});

use componentized::http::client as http;
use exports::composable::templated_http_client::client::{
    Error, Guest, Method, Response, TemplateValue,
};

use expand::Value;

struct Client;

impl Guest for Client {
    async fn request(
        method: Method,
        template: String,
        values: wit_bindgen::rt::Map<String, TemplateValue>,
        headers: wit_bindgen::rt::Map<String, String>,
    ) -> Result<Response, Error> {
        let values = values
            .into_iter()
            .map(|(name, value)| (name, expandable(value)))
            .collect();
        let url = expand::expand(&template, &values).map_err(Error::Failed)?;

        let response = http::request(
            match method {
                Method::Get => http::Method::Get,
                Method::Head => http::Method::Head,
                Method::Delete => http::Method::Delete,
                Method::Options => http::Method::Options,
            },
            url.clone(),
            headers.into_iter().collect(),
            None,
            None,
        )
        .await
        .map_err(|error| Error::Failed(describe(&url, error)))?;

        // The trailers are not read, so their future is dropped.
        let http::HttpResponse {
            status,
            headers,
            body,
            trailers,
        } = response;
        drop(trailers);
        let body = String::from_utf8(body.collect().await)
            .map_err(|_| Error::Failed(format!("the response from {url} is not UTF-8")))?;

        let response = Response {
            status,
            headers,
            body,
        };
        match status {
            200..=299 => Ok(response),
            _ => Err(Error::Response(response)),
        }
    }
}

/// A template value as expected by the expansion.
fn expandable(value: TemplateValue) -> Value {
    match value {
        TemplateValue::Text(text) => Value::Text(text),
        TemplateValue::Signed(n) => Value::Signed(n),
        TemplateValue::Unsigned(n) => Value::Unsigned(n),
        TemplateValue::Float(n) => Value::Float(n),
        TemplateValue::Boolean(b) => Value::Boolean(b),
    }
}

/// Why a request to `url` got no usable response.
fn describe(url: &str, error: http::ErrorCode) -> String {
    match error {
        http::ErrorCode::RedirectLimitExceeded((_, followed)) => {
            format!("{url} redirected past the limit, after {followed} redirects")
        }
        http::ErrorCode::RedirectRequiresBody(_) => {
            format!("{url} redirected with a request to send the body again")
        }
        http::ErrorCode::Other(Some(message)) => format!("{url}: {message}"),
        http::ErrorCode::Other(None) => format!("the request to {url} failed"),
    }
}

export!(Client);
