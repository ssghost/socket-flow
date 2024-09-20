use crate::connection::WSConnection;
use crate::error::Error;
use crate::message::Message;
use crate::read::ReadStream;
use crate::request::{parse_to_http_request, RequestExt};
use crate::write::{Writer, WriterKind};
use base64::prelude::BASE64_STANDARD;
use base64::prelude::*;
use httparse::{Request, EMPTY_HEADER};
use rand::random;
use sha1::{Digest, Sha1};
use std::sync::Arc;
use tokio::io::{split, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc::channel;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

const UUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const SWITCHING_PROTOCOLS: &str = "101 Switching Protocols";

const HTTP_ACCEPT_RESPONSE: &str = "HTTP/1.1 101 Switching Protocols\r\n\
        Connection: Upgrade\r\n\
        Upgrade: websocket\r\n\
        Sec-WebSocket-Accept: {}\r\n\
        \r\n";

pub type Result = std::result::Result<WSConnection, Error>;

/// Used for accepting websocket connections as a server.
///
/// It basically does the first step of verifying the client key in the request
/// going to the second step, which is sending the accept response,
/// finally creating the connection, and returning a `WSConnection`
pub async fn accept_async(stream: TcpStream) -> Result {
    let (reader, mut write_half) = split(stream);
    let mut buf_reader = BufReader::new(reader);

    parse_handshake(&mut buf_reader, &mut write_half).await?;

    // Identify permessage-deflate for enabling compression
    second_stage_handshake(buf_reader, write_half, WriterKind::Server).await
}

async fn second_stage_handshake(
    buf_reader: BufReader<ReadHalf<TcpStream>>,
    write_half: WriteHalf<TcpStream>,
    kind: WriterKind,
) -> Result {
    // This writer instance would be used for writing frames into the socket.
    // Since it's going to be used by two different instances, we need to wrap it through an Arc
    let writer = Arc::new(Mutex::new(Writer::new(write_half, kind)));

    let stream_writer = writer.clone();

    // ReadStream will be running on a separate task, capturing all the incoming frames from the connection, and broadcasting them through this
    // tokio mpsc channel. Therefore, it can be consumed by the end-user of this library
    let (read_tx, read_rx) = channel::<std::result::Result<Message, Error>>(20);
    let mut read_stream = ReadStream::new(buf_reader, read_tx, stream_writer);

    let connection_writer = writer.clone();
    // Transforming the receiver of the channel into a Stream, so we could leverage using
    // next() method, for processing the values from this channel
    let receiver_stream = ReceiverStream::new(read_rx);

    // The WSConnection is the structure that will be delivered to the end-user, which contains
    // a stream of frames, for consuming the incoming frames, and methods for writing frames into
    // the socket
    let ws_connection = WSConnection::new(connection_writer, receiver_stream);

    // Spawning poll_messages which is the method for reading the frames from the socket concurrently,
    // because we need this method running, while the end-user can have
    // a connection returned, for receiving and sending messages.
    // Since this is the only task that holds the ownership of BufReader, if some IO error happens,
    // poll_messages will return.
    // BufReader will be dropped, hence, the writeHalf and TCP connection
    tokio::spawn(async move {
        if let Err(err) = read_stream.poll_messages().await {
            let _ = read_stream.read_tx.send(Err(err)).await;
        }
    });

    Ok(ws_connection)
}

/// Used for connecting as a client to a websocket endpoint.
///
/// It basically does the first step of genereating the client key
/// going to the second step, which is parsing the server reponse,
/// finally creating the connection, and returning a `WSConnection`
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

    second_stage_handshake(buf_reader, write_half, WriterKind::Client).await
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

async fn parse_handshake(
    buf_reader: &mut BufReader<ReadHalf<TcpStream>>,
    write_half: &mut WriteHalf<TcpStream>,
) -> std::result::Result<(), Error> {
    // Using a 1024 sized buffer, because since this is an opening handshake request,
    // there won't be any cases where we have big requests, which also prevents malicious
    // connections that sends a lot of data
    let mut buffer = vec![0; 1024];

    // Read the request into the buffer
    let n = buf_reader.read(&mut buffer).await?;

    // Parse the HTTP request from the buffer
    let mut headers = [EMPTY_HEADER; 16];
    let mut req = Request::new(&mut headers);

    req.parse(&buffer[..n])?;

    // Validate the WebSocket handshake
    if req.method != Some("GET") || req.version != Some(1) {
        return Err(Error::InvalidHTTPHandshake);
    }

    // if req.get_header_value("Connection") != Some(String::from("Upgrade")) {
    //     return Err(Error::NoConnectionHeaderPresent);
    // }
    //
    // if req.get_header_value("Upgrade") != Some(String::from("websocket")) {
    //     return Err(Error::NoUpgradeHeaderPresent);
    // }

    if req.get_header_value("Host").is_none() {
        return Err(Error::NoHostHeaderPresent);
    }

    let sec_websocket_key = match req.get_header_value("Sec-WebSocket-Key") {
        Some(key) => key.to_string(),
        None => Err(Error::NoSecWebsocketKey)?,
    };

    let accept_key = generate_websocket_accept_value(sec_websocket_key);

    let response = HTTP_ACCEPT_RESPONSE.replace("{}", &accept_key);
    write_half
        .write_all(response.as_bytes())
        .await
        .map_err(|source| Error::IOError { source })?;
    write_half.flush().await?;

    Ok(())
}
