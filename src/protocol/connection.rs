use crate::client::RedisClientInner;
use crate::error::{RedisError, RedisErrorKind};
use crate::multiplexer::{Counters, SentCommand};
use crate::protocol::codec::RedisCodec;
use crate::protocol::types::{ClusterKeyCache, RedisCommand, RedisCommandKind};
use crate::protocol::utils as protocol_utils;
use crate::trace;
use crate::types::ClientState;
use crate::types::Resolve;
use crate::utils as client_utils;
use futures::sink::SinkExt;
use futures::stream::{SplitSink, SplitStream, StreamExt};
use redis_protocol::types::Frame as ProtocolFrame;
use std::net::SocketAddr;

use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

#[cfg(feature = "enable-tls")]
use crate::protocol::tls;
#[cfg(feature = "enable-tls")]
use tokio_native_tls::TlsStream;

/// The contents of a simplestring OK response.
pub const OK: &'static str = "OK";

pub type FramedTcp = Framed<TcpStream, RedisCodec>;
#[cfg(feature = "enable-tls")]
pub type FramedTls = Framed<TlsStream<TcpStream>, RedisCodec>;
#[cfg(not(feature = "enable-tls"))]
pub type FramedTls = FramedTcp;

pub type TcpRedisReader = SplitStream<FramedTcp>;
pub type TcpRedisWriter = SplitSink<FramedTcp, ProtocolFrame>;

pub type TlsRedisReader = SplitStream<FramedTls>;
pub type TlsRedisWriter = SplitSink<FramedTls, ProtocolFrame>;

pub enum RedisStream {
  Tls(TlsRedisReader),
  Tcp(TcpRedisReader),
}

pub enum RedisSink {
  Tls(TlsRedisWriter),
  Tcp(TcpRedisWriter),
}

pub async fn request_response<T>(
  mut transport: Framed<T, RedisCodec>,
  request: &RedisCommand,
) -> Result<(ProtocolFrame, Framed<T, RedisCodec>), RedisError>
where
  T: AsyncRead + AsyncWrite + Unpin + 'static,
{
  let frame = request.to_frame()?;
  let _ = transport.send(frame).await?;
  let (response, transport) = transport.into_future().await;

  let response = match response {
    Some(result) => result?,
    None => ProtocolFrame::Null,
  };
  Ok((response, transport))
}

pub async fn authenticate<T>(
  transport: Framed<T, RedisCodec>,
  name: &str,
  key: Option<String>,
) -> Result<Framed<T, RedisCodec>, RedisError>
where
  T: AsyncRead + AsyncWrite + Unpin + 'static,
{
  let transport = if let Some(key) = key {
    let command = RedisCommand::new(RedisCommandKind::Auth, vec![key.into()], None);
    debug!("{}: Authenticating Redis client...", name);
    let (response, transport) = request_response(transport, &command).await?;

    if let ProtocolFrame::SimpleString(inner) = response {
      if inner == OK {
        transport
      } else {
        return Err(RedisError::new(RedisErrorKind::Auth, inner));
      }
    } else {
      return Err(RedisError::new(
        RedisErrorKind::ProtocolError,
        format!("Invalid auth response {:?}.", response),
      ));
    }
  } else {
    transport
  };

  debug!("{}: Changing client name to {}", name, name);
  let command = RedisCommand::new(RedisCommandKind::ClientSetname, vec![name.into()], None);
  let (response, transport) = request_response(transport, &command).await?;

  if let ProtocolFrame::SimpleString(inner) = response {
    if inner == OK {
      debug!("{}: Successfully set Redis client name.", name);
      Ok(transport)
    } else {
      Err(RedisError::new(RedisErrorKind::ProtocolError, inner))
    }
  } else {
    Err(RedisError::new(
      RedisErrorKind::ProtocolError,
      format!("Failed to set client name: {:?}.", response),
    ))
  }
}

#[cfg(feature = "enable-tls")]
pub async fn create_authenticated_connection_tls(
  addr: &SocketAddr,
  domain: &str,
  inner: &Arc<RedisClientInner>,
) -> Result<FramedTls, RedisError> {
  let server = format!("{}:{}", addr.ip().to_string(), addr.port());
  let codec = RedisCodec::new(inner, server);
  let client_name = inner.client_name();
  let auth_key = inner.config.read().key();

  let socket = TcpStream::connect(addr).await?;
  let tls_stream = tls::create_tls_connector(&inner.config)?;
  let socket = tls_stream.connect(domain, socket).await?;
  let framed = authenticate(Framed::new(socket, codec), &client_name, auth_key).await?;

  client_utils::set_client_state(&inner.state, ClientState::Connected);
  Ok(framed)
}

#[cfg(not(feature = "enable-tls"))]
pub(crate) async fn create_authenticated_connection_tls(
  addr: &SocketAddr,
  _domain: &str,
  inner: &Arc<RedisClientInner>,
) -> Result<FramedTls, RedisError> {
  create_authenticated_connection(addr, inner).await
}

pub async fn create_authenticated_connection(
  addr: &SocketAddr,
  inner: &Arc<RedisClientInner>,
) -> Result<FramedTcp, RedisError> {
  let server = format!("{}:{}", addr.ip().to_string(), addr.port());
  let codec = RedisCodec::new(inner, server);
  let client_name = inner.client_name();
  let auth_key = inner.config.read().key();

  let socket = TcpStream::connect(addr).await?;
  let framed = authenticate(Framed::new(socket, codec), &client_name, auth_key).await?;

  client_utils::set_client_state(&inner.state, ClientState::Connected);
  Ok(framed)
}

async fn read_cluster_state(
  inner: &Arc<RedisClientInner>,
  host: String,
  port: u16,
  uses_tls: bool,
) -> Option<ClusterKeyCache> {
  let command = RedisCommand::new(RedisCommandKind::ClusterNodes, vec![], None);

  let addr = match inner.resolver.resolve(host.clone(), port).await {
    Ok(addr) => addr,
    Err(e) => {
      _debug!(inner, "Resolver error: {:?}", e);
      return None;
    }
  };

  let response = if uses_tls {
    let connection = match create_authenticated_connection_tls(&addr, &host, &inner).await {
      Ok(connection) => connection,
      Err(e) => {
        _debug!(inner, "Error creating tls connection to {}:{} => {:?}", host, port, e);
        return None;
      }
    };

    match request_response(connection, &command).await {
      Ok((frame, _)) => frame,
      Err(e) => {
        _trace!(inner, "Failed to read cluster state from {}:{} => {:?}", host, port, e);
        return None;
      }
    }
  } else {
    let connection = match create_authenticated_connection(&addr, &inner).await {
      Ok(connection) => connection,
      Err(e) => {
        _debug!(inner, "Error creating connection to {}:{} => {:?}", host, port, e);
        return None;
      }
    };

    match request_response(connection, &command).await {
      Ok((frame, _)) => frame,
      Err(e) => {
        _trace!(inner, "Failed to read cluster state from {}:{} => {:?}", host, port, e);
        return None;
      }
    }
  };

  if response.is_error() {
    _trace!(
      inner,
      "Protocol error reading cluster state from {}:{} => {:?}",
      host,
      port,
      response
    );
    return None;
  }
  let cluster_state = match response.to_string() {
    Some(response) => response,
    None => return None,
  };

  _trace!(inner, "Cluster state:\n {}", cluster_state);
  if let Ok(cache) = ClusterKeyCache::new(Some(cluster_state)) {
    return Some(cache);
  }

  None
}

pub async fn read_cluster_nodes(inner: &Arc<RedisClientInner>) -> Result<ClusterKeyCache, RedisError> {
  let known_nodes = protocol_utils::read_clustered_hosts(&inner.config)?;
  let uses_tls = inner.config.read().tls().is_some();

  for (host, port) in known_nodes.into_iter() {
    _debug!(inner, "Attempting to read cluster state from {}:{}", host, port);

    if let Some(cache) = read_cluster_state(inner, host, port, uses_tls).await {
      return Ok(cache);
    }
  }

  Err(RedisError::new(
    RedisErrorKind::Unknown,
    "Could not read cluster state from any known node in the cluster.",
  ))
}

pub async fn write_command(
  inner: &Arc<RedisClientInner>,
  sink: &mut RedisSink,
  counters: &Counters,
  command: &mut SentCommand,
) -> Result<(), RedisError> {
  let frame = command.command.to_frame()?;
  command.command.incr_attempted();
  command.network_start = Some(Instant::now());

  // flush the socket under the following conditions:
  // * we don't know of any queued commands following this command
  // * we've fed up to the global max feed count commands already
  // * the command closes the connection
  // * the command ends a transaction
  // * the command blocks the multiplexer command loop
  let should_flush = counters.should_send()
    || command.command.is_quit()
    || command.command.kind.ends_transaction()
    || client_utils::is_locked_some(&command.command.resp_tx);

  if should_flush {
    _trace!(inner, "Sending command and flushing the sink.");
    trace::set_network_span(&mut command.command, true);

    match sink {
      RedisSink::Tcp(ref mut inner) => inner.send(frame).await?,
      RedisSink::Tls(ref mut inner) => inner.send(frame).await?,
    };
    counters.reset_feed_count();
  } else {
    _trace!(inner, "Sending command without flushing the sink.");
    trace::set_network_span(&mut command.command, false);

    match sink {
      RedisSink::Tcp(ref mut inner) => inner.feed(frame).await?,
      RedisSink::Tls(ref mut inner) => inner.feed(frame).await?,
    };
    counters.incr_feed_count();
  };
  counters.incr_in_flight();

  Ok(())
}