use crate::connection::WSConnection;
use crate::error::Error;
use crate::request::parse_to_http_request;
use base64::prelude::BASE64_STANDARD;
use base64::prelude::*;
use bytes::BytesMut;
use rand::random;
use sha1::{Digest, Sha1};
use tokio::io::{split, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use crate::write::Writer;

const SEC_WEBSOCKETS_KEY: &str = "Sec-WebSocket-Key:";
const UUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const SWITCHING_PROTOCOLS: &str = "101 Switching Protocols";

const HTTP_ACCEPT_RESPONSE: &str = "HTTP/1.1 101 Switching Protocols\r\n\
        Connection: Upgrade\r\n\
        Upgrade: websocket\r\n\
        Sec-WebSocket-Accept: {}\r\n\
        \r\n";

pub type Result = std::result::Result<WSConnection, Error>;

// Using Send trait because we are going to run the process to read frames from the socket concurrently
// TCPStream from tokio implements Send
// Using static, because tokio::spawn returns a JoinHandle, because the spawned task could outilive the
// lifetime of the function call to tokio::spawn.
pub async fn accept_async(stream: TcpStream) -> Result {
    let (reader, mut write_half) = split(stream);
    let mut buf_reader = BufReader::new(reader);

    let sec_websockets_accept = header_read(&mut buf_reader).await;

    match sec_websockets_accept {
        Some(accept_value) => {
            let response = HTTP_ACCEPT_RESPONSE.replace("{}", &accept_value);
            write_half
                .write_all(response.as_bytes())
                .await
                .map_err(|source| Error::IOError { source })?
        }
        None => Err(Error::NoSecWebsocketKey)?,
    }

    second_stage_handshake(buf_reader, write_half).await
}

async fn second_stage_handshake(
    buf_reader: BufReader<ReadHalf<TcpStream>>,
    write_half: WriteHalf<TcpStream>,
) -> Result {
    // We are using tokio async channels to communicate the frames received from the client
    // and another channel to send messages from server to client;
    // all the callers that are trying to add new data will be blocked
    // until we have free space
    // (off course, using await in the method)

    // We are separating the stream in read and write,
    // because handling them in the same struct would need us to
    // wrap some references with Arc<mutex>,
    // and for the sake of a clean syntax, we selected to split it
    //
    // let stream_writer = writer_arc.clone();
    // let read_stream = ReadStream::new(buf_reader, stream_writer);

    let ws_connection = WSConnection::new(buf_reader, write_half);

    // We are spawning poll_messages which is the method for reading the frames from the socket
    // we need to do it concurrently, because we need this method running, while the end-user can have
    // a channel returned, for receiving and sending messages
    // Since ReadHalf and WriteHalf implements Send and Sync, it's ok to send them over spawn
    // Additionally, since our BufReader doesn't change, we only call read methods from it, there is no
    // need to wrap it in an Arc<Mutex>, also because poll_messages read frames sequentially.
    // Also, since this is the only task that holds the ownership of BufReader, if some IO error happens,
    // poll_messages will return. Since BufReader is only inside the scope of the function, it will be dropped
    //  the WriteHalf, hence, the TCP connection

    // tokio::spawn(async move {
    //     if let Err(err) = read_stream.poll_messages().await {
    //         read_tx_r.lock().await.send(Err(err)).await.unwrap();
    //     }
    // });

    Ok(ws_connection)
}

pub async fn connect_async(addr: &str) -> Result {
    let client_websocket_key = generate_websocket_key();
    let (request, hostname) = parse_to_http_request(addr, &client_websocket_key)?;

    let stream = TcpStream::connect(hostname).await?;

    let (reader, mut write_half) = split(stream);
    let mut buf_reader = BufReader::new(reader);

    write_half.write_all(request.as_bytes()).await?;

    // Create a buffer for the server's response, since most of the Websocket won't send a big payload
    // for the handshake response, defining this size of Vector would be enough, and also will put a limit
    // to bigger payloads
    let mut buffer: Vec<u8> = vec![0; 206];

    // Read the server's response
    let number_read = buf_reader.read(&mut buffer).await?;

    // Keep only the section of the buffer that was filled.
    buffer.truncate(number_read);

    // Convert the server's response to a string
    let response = String::from_utf8(buffer)?;

    // Verify that the server agreed to upgrade the connection
    if !response.contains(SWITCHING_PROTOCOLS) {
        return Err(Error::NoUpgrade);
    }

    // Generate the server expected accept key using UUID, and checking if it's present in the response
    let expected_accept_value = generate_websocket_accept_value(client_websocket_key);
    if !response.contains(&expected_accept_value) {
        return Err(Error::InvalidAcceptKey);
    }

    second_stage_handshake(buf_reader, write_half).await
}

// Here we are using the generic T, and expressing its two tokio traits, to avoiding adding the
// entire type of the argument in the function signature (BufReader<ReadHalf<TcpStream>>)
// The Unpin trait in Rust is used when the exact location of an object in memory needs to remain
// constant after being pinned. In simple terms, it means that the object doesn't move around in memory
// Here, we need to use Unpin, because the timeout function puts the passed Future into a Pin<Box<dyn Future>>
async fn header_read<T: AsyncReadExt + Unpin>(buf_reader: &mut T) -> Option<String> {
    let mut websocket_header: Option<String> = None;
    let mut websocket_accept: Option<String> = None;
    let mut header_buf = BytesMut::with_capacity(1024 * 16); // 16 kilobytes

    // Limit the maximum amount of data read to prevent a denial of service attack.
    while header_buf.len() <= 1024 * 16 {
        let mut tmp_buf = vec![0; 1024];
        match timeout(Duration::from_secs(10), buf_reader.read(&mut tmp_buf)).await {
            Ok(Ok(0)) | Err(_) => break, // EOF reached or Timeout, we stop. In the case of EOF
            // there is no need to log or return EOF or timeout errors
            Ok(Ok(n)) => {
                header_buf.extend_from_slice(&tmp_buf[..n]);
                let s = String::from_utf8_lossy(&header_buf);
                if let Some(start) = s.find(SEC_WEBSOCKETS_KEY) {
                    websocket_header = Some(s[start..].lines().next().unwrap().to_string());
                    break;
                }
            }
            _ => {}
        }
    }

    if let Some(header) = websocket_header {
        if let Some(key) = parse_websocket_key(header) {
            websocket_accept = Some(generate_websocket_accept_value(key));
        }
    }

    websocket_accept
}

fn parse_websocket_key(header: String) -> Option<String> {
    for line in header.lines() {
        if line.starts_with(SEC_WEBSOCKETS_KEY) {
            if let Some(stripped) = line.strip_prefix(SEC_WEBSOCKETS_KEY) {
                return stripped.split_whitespace().next().map(ToOwned::to_owned);
            }
        }
    }
    None
}

fn generate_websocket_accept_value(key: String) -> String {
    let mut sha1 = Sha1::new();
    sha1.update(key.as_bytes());
    sha1.update(UUID.as_bytes());
    BASE64_STANDARD.encode(sha1.finalize())
}

fn generate_websocket_key() -> String {
    let random_bytes: [u8; 16] = random();
    BASE64_STANDARD.encode(random_bytes)
}
