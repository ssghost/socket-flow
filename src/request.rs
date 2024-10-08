use crate::error::Error;
use url::Url;

// Function used for client connection, parsing the ws/wss URL to http, for constructing the
// handshake request, which includes the sec-websockets-key, the URL path, scheme and another relevant
// info. This function also returns the hostname since this is necessary for establishing the TCP socket
pub fn parse_to_http_request(ws_url: &str, key: &str) -> Result<(String, String, String, bool), Error> {
    let parsed_url = Url::parse(ws_url)?;
    let mut use_tls = false;

    let http_port: u16 = match parsed_url.scheme() {
        "ws" => 80,
        "wss" => {
            use_tls = true;
            443
        }
        _ => return Err(Error::InvalidSchemeURL),
    };

    let host = parsed_url.host_str().ok_or(Error::URLNoHost)?;
    let port = parsed_url.port().unwrap_or(http_port);

    let host_with_port = format!("{}:{}", host, port);

    let request_path = match parsed_url.query() {
        Some(query) => format!("{}?{}", parsed_url.path(), query),
        None => parsed_url.path().to_string(),
    };

    // Since we already have all the info, it isn't worth converting everything to a HTTP request type
    // and considering everything is bits into the TCP packets, we simply manipulate the string, and
    // convert it to bytes when sending to the server
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        request_path,
        host,
        key,
    );

    Ok((request, host_with_port, String::from(host), use_tls))
}

pub trait RequestExt {
    fn get_header_value(&self, header_name: &str) -> Option<String>;
}

impl<'a, 'b> RequestExt for httparse::Request<'a, 'b> {
    fn get_header_value(&self, header_name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(header_name))
            .map(|header| String::from_utf8_lossy(header.value).to_string())
    }
}
