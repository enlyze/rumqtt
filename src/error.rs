use std::result;
use std::io;
use std::sync::mpsc::{self, RecvError};

use mio::timer::TimerError;
use mio::channel::SendError;
use openssl::ssl;
use mqtt::topic_name::TopicNameError;
use mqtt::topic_filter::TopicFilterError;
use mqtt::packet::*;
use mqtt::control::variable_header::ConnectReturnCode;


pub type SslError = ssl::error::SslError;
pub type Result<T> = result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    MqttEncode,
    Tls(i32),
    Subscribe(i32),
    Publish(i32),
    AlreadyConnected,
    UnrecognizedPacket,
    ConnectionAbort,
    HandshakeFailed,
    Disconnected,
    Timeout,
    ConnectionRefused(ConnectReturnCode),
    Io(io::Error),
    InvalidCert(String),
    SendError,
    Recv(RecvError),
    Timer(TimerError),
    NoStream,
    TopicName,
    TopicFilter,
    NoReconnectTry,
    MqttPacket,
    Ssl(SslError),
    EventLoop,
    Read,
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error { Error::Io(err) }
}

impl From<TopicNameError> for Error {
    fn from(_: TopicNameError) -> Error { Error::TopicName }
}

impl From<TopicFilterError> for Error {
    fn from(_: TopicFilterError) -> Error { Error::TopicFilter }
}

impl<T> From<SendError<T>> for Error {
    fn from(_: SendError<T>) -> Error { Error::SendError }
}

impl<T> From<mpsc::SendError<T>> for Error {
    fn from(_: mpsc::SendError<T>) -> Error { Error::SendError }
}

impl From<RecvError> for Error {
    fn from(err: RecvError) -> Error { Error::Recv(err) }
}

impl<'a, P: Packet<'a>> From<PacketError<'a, P>> for Error {
    fn from(_: PacketError<'a, P>) -> Error { Error::MqttPacket }
}

impl From<SslError> for Error {
    fn from(e: SslError) -> Error { Error::Ssl(e) }
}

impl From<TimerError> for Error {
    fn from(e: TimerError) -> Error { Error::Timer(e) }
}
