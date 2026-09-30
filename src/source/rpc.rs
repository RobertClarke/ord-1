//! Blocking JSON-RPC 1.0 client for bitcoind.

use {
  super::{
    RpcAuth, RpcConfig,
    hex::{self, HexBody, HexBodyError},
  },
  anyhow::{Context, anyhow, ensure},
  base64::Engine,
  bitcoin::{BlockHash, block::Header, consensus},
  reqwest::{
    StatusCode,
    blocking::{Client, Response},
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue},
  },
  serde::{Deserialize, Serialize, de::DeserializeOwned},
  serde_json::{Value, value::RawValue},
  std::{
    fmt, fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
      Arc, PoisonError, RwLock,
      atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
  },
};

/// A JSON-RPC error object returned by bitcoind. Errors returned by `Rpc`
/// wrap it when bitcoind answered with one, so callers can inspect the code
/// with `error.downcast_ref::<RpcError>()`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RpcError {
  pub code: i64,
  pub message: String,
}

impl RpcError {
  /// `RPC_IN_WARMUP`: bitcoind is starting; the only error that is retried.
  pub const IN_WARMUP: i64 = -28;
  /// `RPC_INVALID_ADDRESS_OR_KEY`, e.g. "Block not found".
  pub const INVALID_ADDRESS_OR_KEY: i64 = -5;
  /// `RPC_INVALID_PARAMETER`, e.g. "Block height out of range".
  pub const INVALID_PARAMETER: i64 = -8;

  fn is_transient(&self) -> bool {
    self.code == Self::IN_WARMUP
  }
}

impl fmt::Display for RpcError {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{} (RPC error {})", self.message, self.code)
  }
}

impl std::error::Error for RpcError {}

/// How transient failures (connection errors, timeouts, HTTP 5xx without a
/// JSON-RPC error, HTTP 401 with cookie auth, RPC error -28) are retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
  /// Attempts before giving up, including the first.
  pub attempts: u32,
  /// Delay before the first retry; doubled after every failed attempt.
  pub initial_delay: Duration,
  pub max_delay: Duration,
}

impl Default for RetryPolicy {
  /// 10 attempts, 0.1 s doubling up to 10 s: gives up after ~33 s.
  fn default() -> Self {
    Self {
      attempts: 10,
      initial_delay: Duration::from_millis(100),
      max_delay: Duration::from_secs(10),
    }
  }
}

/// Blocking, thread-safe, cheaply cloneable JSON-RPC client with a keep-alive
/// connection pool; retries transient failures (connection reset, 5xx, work
/// queue full) with backoff.
#[derive(Clone)]
pub struct Rpc {
  inner: Arc<Inner>,
}

struct Inner {
  client: Client,
  url: String,
  auth: Auth,
  retry: RetryPolicy,
  next_id: AtomicU64,
}

enum Auth {
  None,
  Fixed(HeaderValue),
  Cookie {
    path: PathBuf,
    header: RwLock<HeaderValue>,
  },
}

impl Auth {
  fn header(&self) -> Option<HeaderValue> {
    match self {
      Self::None => None,
      Self::Fixed(header) => Some(header.clone()),
      Self::Cookie { header, .. } => Some(
        header
          .read()
          .unwrap_or_else(PoisonError::into_inner)
          .clone(),
      ),
    }
  }
}

/// How one attempt failed.
pub(crate) enum Failure {
  /// Worth retrying.
  Transient(anyhow::Error),
  Fatal(anyhow::Error),
}

/// Why a response body could not be turned into a result.
pub(crate) enum ParseError {
  /// The response carries a JSON-RPC error.
  Rpc(RpcError),
  Invalid(anyhow::Error),
}

#[derive(Serialize)]
struct Request<'a> {
  id: u64,
  method: &'a str,
  params: &'a [Value],
}

#[derive(Deserialize)]
struct Envelope<'a> {
  #[serde(borrow, default)]
  result: Option<&'a RawValue>,
  #[serde(default)]
  error: Option<RpcError>,
  #[serde(default)]
  id: Value,
}

impl fmt::Debug for Rpc {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    f.debug_struct("Rpc").field("url", &self.inner.url).finish()
  }
}

impl Rpc {
  pub fn new(config: RpcConfig) -> anyhow::Result<Self> {
    Self::with_retry_policy(config, RetryPolicy::default())
  }

  pub fn with_retry_policy(config: RpcConfig, retry: RetryPolicy) -> anyhow::Result<Self> {
    ensure!(
      retry.attempts >= 1,
      "retry policy needs at least one attempt"
    );

    let url = if config.url.contains("://") {
      config.url
    } else {
      format!("http://{}", config.url)
    };
    reqwest::Url::parse(&url).with_context(|| format!("invalid bitcoind RPC URL `{url}`"))?;

    let auth = match config.auth {
      RpcAuth::None => Auth::None,
      RpcAuth::UserPass { username, password } => {
        Auth::Fixed(basic_auth(&format!("{username}:{password}"))?)
      }
      RpcAuth::Cookie(path) => Auth::Cookie {
        header: RwLock::new(read_cookie(&path)?),
        path,
      },
    };

    let timeout = (config.timeout_secs > 0).then(|| Duration::from_secs(config.timeout_secs));

    let client = Client::builder()
      .no_proxy()
      .timeout(timeout)
      .connect_timeout(timeout.map_or(CONNECT_TIMEOUT, |timeout| timeout.min(CONNECT_TIMEOUT)))
      // Below bitcoind's default `-rpcservertimeout` (30 s), so pooled
      // connections are dropped before the server closes them.
      .pool_idle_timeout(Duration::from_secs(20))
      .pool_max_idle_per_host(256)
      .tcp_nodelay(true)
      .build()
      .context("building the RPC HTTP client")?;

    Ok(Self {
      inner: Arc::new(Inner {
        client,
        url,
        auth,
        retry,
        next_id: AtomicU64::new(1),
      }),
    })
  }

  /// `getblockchaininfo.chain`: "main", "test", "testnet4", "signet", "regtest".
  pub fn chain(&self) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct Info {
      chain: String,
    }
    Ok(self.call::<Info>("getblockchaininfo", &[])?.chain)
  }

  pub fn block_count(&self) -> anyhow::Result<u32> {
    self.call("getblockcount", &[])
  }

  pub fn best_block_hash(&self) -> anyhow::Result<BlockHash> {
    self.call("getbestblockhash", &[])
  }

  pub fn block_hash(&self, height: u32) -> anyhow::Result<BlockHash> {
    self.call("getblockhash", &[height.into()])
  }

  /// Hashes of the blocks at `heights`, in one batch request.
  pub fn block_hashes(&self, heights: Range<u32>) -> anyhow::Result<Vec<BlockHash>> {
    let params = heights
      .map(|height| vec![Value::from(height)])
      .collect::<Vec<Vec<Value>>>();
    self.batch("getblockhash", &params)
  }

  pub fn block_header(&self, hash: BlockHash) -> anyhow::Result<Header> {
    let text = self.call::<String>("getblockheader", &[hash.to_string().into(), false.into()])?;
    let bytes = hex::decode(text.as_bytes()).context("decoding getblockheader hex")?;
    let header = consensus::deserialize::<Header>(&bytes).context("decoding block header")?;
    ensure!(
      header.block_hash() == hash,
      "getblockheader {hash} returned the header of block {}",
      header.block_hash()
    );
    Ok(header)
  }

  /// `getblock <hash> 0`, decoded.
  pub fn block_raw(&self, hash: BlockHash) -> anyhow::Result<Vec<u8>> {
    self.call_hex("getblock", &[hash.to_string().into(), 0.into()])
  }

  /// One JSON-RPC call; `T` is deserialized from `result` (`null` when the
  /// result is null or missing).
  pub fn call<T: DeserializeOwned>(&self, method: &str, params: &[Value]) -> anyhow::Result<T> {
    let id = self.reserve_ids(1);
    let body = request_body(id, method, params);
    self.retry(method, || {
      let (status, bytes) = self.post(&body)?;
      parse_single(status, &bytes, id)
    })
  }

  /// Calls `method` once per entry of `params` in a single batch request;
  /// results are in `params` order. Fails if any call fails.
  pub fn batch<T: DeserializeOwned>(
    &self,
    method: &str,
    params: &[Vec<Value>],
  ) -> anyhow::Result<Vec<T>> {
    if params.is_empty() {
      return Ok(Vec::new());
    }
    let first = self.reserve_ids(params.len());
    let requests = params
      .iter()
      .zip(first..)
      .map(|(params, id)| Request { id, method, params })
      .collect::<Vec<Request>>();
    let body = serde_json::to_vec(&requests).expect("JSON values serialize");
    self.retry(method, || {
      let (status, bytes) = self.post(&body)?;
      parse_batch(status, &bytes, first, params.len())
    })
  }

  /// A call whose response body is handed to `parse` as is (for large
  /// results parsed into borrowed structures). `parse` must report JSON-RPC
  /// errors as `ParseError::Rpc` so they are classified for retrying.
  pub(crate) fn call_parsed<T>(
    &self,
    method: &str,
    params: &[Value],
    parse: impl Fn(&[u8]) -> Result<T, ParseError>,
  ) -> anyhow::Result<T> {
    let id = self.reserve_ids(1);
    let body = request_body(id, method, params);
    self.retry(method, || {
      let (status, bytes) = self.post(&body)?;
      match parse(&bytes) {
        Ok(value) if status.is_success() => Ok(value),
        Ok(_) => Err(Failure::Fatal(anyhow!("HTTP {status}"))),
        Err(ParseError::Rpc(error)) => Err(rpc_failure(error)),
        Err(ParseError::Invalid(error)) => Err(match parse_single::<Value>(status, &bytes, id) {
          Err(failure) => failure,
          Ok(_) => Failure::Fatal(error.context("unexpected response")),
        }),
      }
    })
  }

  /// A call whose result is a hex string, decoded while it streams in.
  fn call_hex(&self, method: &str, params: &[Value]) -> anyhow::Result<Vec<u8>> {
    let id = self.reserve_ids(1);
    let body = request_body(id, method, params);
    self.retry(method, || {
      let mut response = self.send(&body)?;
      let status = response.status();
      let size_hint = response.content_length();
      match hex::read_hex_result(&mut response, size_hint) {
        Ok(HexBody::Hex { bytes, envelope }) => {
          parse_single::<Value>(status, &envelope, id)?;
          Ok(bytes)
        }
        Ok(HexBody::Other(body)) => {
          parse_single::<Value>(status, &body, id)?;
          Err(Failure::Fatal(anyhow!(
            "expected a hex string result, got `{}`",
            snippet(&body)
          )))
        }
        Err(HexBodyError::Io(error)) => Err(Failure::Transient(
          anyhow::Error::new(error).context("reading the response"),
        )),
        Err(HexBodyError::Malformed(message)) => {
          Err(Failure::Fatal(anyhow!("malformed response: {message}")))
        }
      }
    })
  }

  fn reserve_ids(&self, n: usize) -> u64 {
    self.inner.next_id.fetch_add(n as u64, Ordering::Relaxed)
  }

  /// Sends one request and reads the whole body.
  fn post(&self, body: &[u8]) -> Result<(StatusCode, Vec<u8>), Failure> {
    let response = self.send(body)?;
    let status = response.status();
    let bytes = response.bytes().map_err(|error| {
      Failure::Transient(anyhow::Error::new(error).context("reading the response"))
    })?;
    Ok((status, bytes.into()))
  }

  /// Sends one request; handles the statuses that say nothing about the
  /// call itself (401, 503).
  fn send(&self, body: &[u8]) -> Result<Response, Failure> {
    let mut request = self
      .inner
      .client
      .post(&self.inner.url)
      .header(CONTENT_TYPE, "application/json")
      .body(body.to_vec());

    if let Some(header) = self.inner.auth.header() {
      request = request.header(AUTHORIZATION, header);
    }

    let response = request.send().map_err(|error| {
      let fatal = error.is_builder();
      let error = anyhow::Error::new(error).context(format!("POST {}", self.inner.url));
      if fatal {
        Failure::Fatal(error)
      } else {
        Failure::Transient(error)
      }
    })?;

    match response.status() {
      StatusCode::UNAUTHORIZED => Err(self.unauthorized()),
      StatusCode::SERVICE_UNAVAILABLE => Err(Failure::Transient(anyhow!(
        "bitcoind is busy (HTTP 503: {})",
        snippet(&response.bytes().unwrap_or_default())
      ))),
      _ => Ok(response),
    }
  }

  fn unauthorized(&self) -> Failure {
    match &self.inner.auth {
      Auth::Cookie { path, header } => match read_cookie(path) {
        Ok(fresh) => {
          *header.write().unwrap_or_else(PoisonError::into_inner) = fresh;
          Failure::Transient(anyhow!(
            "bitcoind rejected the cookie credentials (HTTP 401); re-read {}",
            path.display()
          ))
        }
        Err(error) => {
          Failure::Transient(error.context("bitcoind rejected the cookie credentials (HTTP 401)"))
        }
      },
      Auth::Fixed(_) => Failure::Fatal(anyhow!(
        "bitcoind rejected the RPC username and password (HTTP 401)"
      )),
      Auth::None => Failure::Fatal(anyhow!("bitcoind requires RPC credentials (HTTP 401)")),
    }
  }

  fn retry<T>(
    &self,
    method: &str,
    mut attempt: impl FnMut() -> Result<T, Failure>,
  ) -> anyhow::Result<T> {
    let policy = self.inner.retry;
    let mut delay = policy.initial_delay;
    let mut n = 1;
    loop {
      match attempt() {
        Ok(value) => return Ok(value),
        Err(Failure::Fatal(error)) => {
          return Err(error.context(format!("bitcoind RPC `{method}` failed")));
        }
        Err(Failure::Transient(error)) => {
          if n >= policy.attempts {
            return Err(
              error.context(format!("bitcoind RPC `{method}` failed after {n} attempts")),
            );
          }
          log::warn!(
            "bitcoind RPC `{method}` attempt {n}/{} failed, retrying in {delay:?}: {error:#}",
            policy.attempts
          );
          thread::sleep(delay);
          delay = (delay * 2).min(policy.max_delay);
          n += 1;
        }
      }
    }
  }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn basic_auth(credentials: &str) -> anyhow::Result<HeaderValue> {
  let mut header = HeaderValue::from_str(&format!(
    "Basic {}",
    base64::engine::general_purpose::STANDARD.encode(credentials)
  ))
  .context("RPC credentials are not a valid header value")?;
  header.set_sensitive(true);
  Ok(header)
}

fn read_cookie(path: &Path) -> anyhow::Result<HeaderValue> {
  let cookie = fs::read_to_string(path)
    .with_context(|| format!("reading bitcoind cookie file {}", path.display()))?;
  let cookie = cookie.trim_end_matches(['\r', '\n']);
  ensure!(
    cookie.contains(':'),
    "cookie file {} is not `user:password`",
    path.display()
  );
  basic_auth(cookie)
}

fn request_body(id: u64, method: &str, params: &[Value]) -> Vec<u8> {
  serde_json::to_vec(&Request { id, method, params }).expect("JSON values serialize")
}

fn rpc_failure(error: RpcError) -> Failure {
  if error.is_transient() {
    Failure::Transient(anyhow::Error::new(error))
  } else {
    Failure::Fatal(anyhow::Error::new(error))
  }
}

/// A body that is not a JSON-RPC response.
fn unparseable(status: StatusCode, body: &[u8], error: serde_json::Error) -> Failure {
  if status.is_server_error() {
    Failure::Transient(anyhow!("HTTP {status}: {}", snippet(body)))
  } else if status.is_success() {
    Failure::Fatal(
      anyhow::Error::new(error).context(format!("malformed JSON-RPC response `{}`", snippet(body))),
    )
  } else {
    Failure::Fatal(anyhow!("HTTP {status}: {}", snippet(body)))
  }
}

fn parse_single<T: DeserializeOwned>(
  status: StatusCode,
  body: &[u8],
  id: u64,
) -> Result<T, Failure> {
  let envelope =
    serde_json::from_slice::<Envelope>(body).map_err(|error| unparseable(status, body, error))?;

  if let Some(error) = envelope.error {
    return Err(rpc_failure(error));
  }

  if !status.is_success() {
    return Err(Failure::Fatal(anyhow!("HTTP {status}: {}", snippet(body))));
  }

  if envelope.id.as_u64() != Some(id) {
    return Err(Failure::Fatal(anyhow!(
      "response id {} does not match request id {id}",
      envelope.id
    )));
  }

  parse_result(envelope.result)
}

fn parse_result<T: DeserializeOwned>(result: Option<&RawValue>) -> Result<T, Failure> {
  let text = result.map_or("null", RawValue::get);
  serde_json::from_str(text).map_err(|error| {
    Failure::Fatal(
      anyhow::Error::new(error)
        .context(format!("unexpected result `{}`", snippet(text.as_bytes()))),
    )
  })
}

fn parse_batch<T: DeserializeOwned>(
  status: StatusCode,
  body: &[u8],
  first_id: u64,
  n: usize,
) -> Result<Vec<T>, Failure> {
  let envelopes = match serde_json::from_slice::<Vec<Envelope>>(body) {
    Ok(envelopes) => envelopes,
    // A batch rejected as a whole comes back as a single response.
    Err(error) => {
      return Err(match serde_json::from_slice::<Envelope>(body) {
        Ok(Envelope {
          error: Some(error), ..
        }) => rpc_failure(error),
        _ => unparseable(status, body, error),
      });
    }
  };

  if envelopes.len() != n {
    return Err(Failure::Fatal(anyhow!(
      "batch of {n} requests got {} responses",
      envelopes.len()
    )));
  }

  let mut results = (0..n).map(|_| None).collect::<Vec<Option<T>>>();
  let mut first_error: Option<(usize, RpcError)> = None;

  for envelope in envelopes {
    let index = envelope
      .id
      .as_u64()
      .and_then(|id| id.checked_sub(first_id))
      .and_then(|index| usize::try_from(index).ok())
      .filter(|index| *index < n)
      .ok_or_else(|| Failure::Fatal(anyhow!("unexpected response id {}", envelope.id)))?;

    if let Some(error) = envelope.error {
      if error.is_transient() {
        return Err(rpc_failure(error));
      }
      if first_error.as_ref().is_none_or(|(first, _)| index < *first) {
        first_error = Some((index, error));
      }
      continue;
    }

    if results[index].is_some() {
      return Err(Failure::Fatal(anyhow!(
        "duplicate response id {}",
        envelope.id
      )));
    }
    results[index] = Some(parse_result(envelope.result)?);
  }

  if let Some((index, error)) = first_error {
    return Err(Failure::Fatal(
      anyhow::Error::new(error).context(format!("request {index} of the batch")),
    ));
  }

  results
    .into_iter()
    .collect::<Option<Vec<T>>>()
    .ok_or_else(|| Failure::Fatal(anyhow!("batch response is missing results")))
}

/// Start of a response body, for error messages.
fn snippet(body: &[u8]) -> String {
  const MAX: usize = 200;
  let text = String::from_utf8_lossy(&body[..body.len().min(MAX)]);
  let text = text.trim();
  if body.len() > MAX {
    format!("{text}…")
  } else {
    text.into()
  }
}

#[cfg(test)]
mod tests {
  use {
    super::*,
    bitcoin::hashes::Hash,
    std::{
      io::{BufRead, BufReader, Write},
      net::{TcpListener, TcpStream},
      sync::Mutex,
      time::Instant,
    },
  };

  #[derive(Debug, Clone)]
  struct Seen {
    authorization: Option<String>,
    body: String,
  }

  struct Reply {
    status: u16,
    body: String,
    /// Omit Content-Length and close the connection after the body.
    close: bool,
    delay: Duration,
  }

  impl Reply {
    fn json(body: impl Into<String>) -> Self {
      Self::status(200, body)
    }

    fn status(status: u16, body: impl Into<String>) -> Self {
      Self {
        status,
        body: body.into(),
        close: false,
        delay: Duration::ZERO,
      }
    }
  }

  /// A scripted HTTP/1.1 server: `reply(n, request)` answers the n-th
  /// request (0-based).
  struct Server {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
  }

  impl Server {
    fn start(reply: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static) -> Self {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let url = format!("http://{}", listener.local_addr().unwrap());
      let seen = Arc::new(Mutex::new(Vec::new()));
      let reply = Arc::new(reply);
      {
        let seen = seen.clone();
        thread::spawn(move || {
          for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let seen = seen.clone();
            let reply = reply.clone();
            thread::spawn(move || Self::serve(stream, &seen, &*reply));
          }
        });
      }
      Self { url, seen }
    }

    fn serve(stream: TcpStream, seen: &Mutex<Vec<Seen>>, reply: &dyn Fn(usize, &Seen) -> Reply) {
      let mut reader = BufReader::new(stream.try_clone().unwrap());
      let mut stream = stream;
      loop {
        let mut content_length = 0;
        let mut authorization = None;
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
          return;
        }
        loop {
          line.clear();
          if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
          }
          let line = line.trim_end();
          if line.is_empty() {
            break;
          }
          let (name, value) = line.split_once(':').unwrap();
          match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().unwrap(),
            "authorization" => authorization = Some(value.trim().to_string()),
            _ => {}
          }
        }
        let mut body = vec![0; content_length];
        std::io::Read::read_exact(&mut reader, &mut body).unwrap();
        let request = Seen {
          authorization,
          body: String::from_utf8(body).unwrap(),
        };
        let n = {
          let mut seen = seen.lock().unwrap();
          seen.push(request.clone());
          seen.len() - 1
        };
        let reply = reply(n, &request);
        thread::sleep(reply.delay);
        let head = if reply.close {
          format!(
            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
            reply.status
          )
        } else {
          format!(
            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            reply.status,
            reply.body.len()
          )
        };
        if stream.write_all(head.as_bytes()).is_err()
          || stream.write_all(reply.body.as_bytes()).is_err()
          || reply.close
        {
          return;
        }
      }
    }

    fn requests(&self) -> Vec<Seen> {
      self.seen.lock().unwrap().clone()
    }

    fn rpc(&self, auth: RpcAuth) -> Rpc {
      Rpc::with_retry_policy(
        RpcConfig {
          url: self.url.clone(),
          auth,
          timeout_secs: 5,
        },
        fast_retries(4),
      )
      .unwrap()
    }
  }

  fn fast_retries(attempts: u32) -> RetryPolicy {
    RetryPolicy {
      attempts,
      initial_delay: Duration::from_millis(1),
      max_delay: Duration::from_millis(5),
    }
  }

  /// The id of a single request.
  fn id(request: &Seen) -> u64 {
    serde_json::from_str::<Value>(&request.body).unwrap()["id"]
      .as_u64()
      .unwrap()
  }

  fn ok(request: &Seen, result: &str) -> Reply {
    Reply::json(format!(
      "{{\"result\":{result},\"error\":null,\"id\":{}}}",
      id(request)
    ))
  }

  fn rpc_error(request: &Seen, code: i64, message: &str) -> Reply {
    Reply::status(
      500,
      format!(
        "{{\"result\":null,\"error\":{{\"code\":{code},\"message\":\"{message}\"}},\"id\":{}}}",
        id(request)
      ),
    )
  }

  #[test]
  fn requests_are_json_rpc_1_0() {
    let server = Server::start(|_, request| ok(request, "123"));
    let rpc = server.rpc(RpcAuth::None);
    assert_eq!(rpc.block_count().unwrap(), 123);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].authorization, None);
    let body = serde_json::from_str::<Value>(&requests[0].body).unwrap();
    assert_eq!(body["method"], "getblockcount");
    assert_eq!(body["params"], serde_json::json!([]));
    assert!(body.get("jsonrpc").is_none());
  }

  #[test]
  fn retries_busy_warming_up_and_broken_responses() {
    let server = Server::start(|n, request| match n {
      0 => Reply::status(503, "Work queue depth exceeded"),
      1 => rpc_error(request, -28, "Loading block index…"),
      2 => Reply::status(500, "<html>oops</html>"),
      _ => ok(request, "7"),
    });
    let rpc = server.rpc(RpcAuth::None);
    assert_eq!(rpc.block_count().unwrap(), 7);
    assert_eq!(server.requests().len(), 4);
  }

  #[test]
  fn gives_up_after_the_configured_attempts() {
    let server = Server::start(|_, _| Reply::status(503, "Work queue depth exceeded"));
    let error = server.rpc(RpcAuth::None).block_count().unwrap_err();
    assert_eq!(server.requests().len(), 4);
    assert!(
      format!("{error:#}").contains("failed after 4 attempts"),
      "{error:#}"
    );
  }

  #[test]
  fn logic_errors_are_not_retried() {
    let server = Server::start(|_, request| rpc_error(request, -8, "Block height out of range"));
    let error = server.rpc(RpcAuth::None).block_hash(1_000_000).unwrap_err();
    assert_eq!(server.requests().len(), 1);
    let rpc_error = error.downcast_ref::<RpcError>().unwrap();
    assert_eq!(rpc_error.code, RpcError::INVALID_PARAMETER);
    assert_eq!(rpc_error.message, "Block height out of range");

    // The same error with HTTP 200 (JSON-RPC 2.0 servers, mockcore).
    let server = Server::start(|_, request| {
      Reply::json(format!(
        "{{\"error\":{{\"code\":-5,\"message\":\"Block not found\"}},\"id\":{}}}",
        id(request)
      ))
    });
    let error = server
      .rpc(RpcAuth::None)
      .block_raw(BlockHash::all_zeros())
      .unwrap_err();
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
      error.downcast_ref::<RpcError>().unwrap().code,
      RpcError::INVALID_ADDRESS_OR_KEY
    );
  }

  #[test]
  fn other_http_errors_are_not_retried() {
    for status in [403, 404] {
      let server = Server::start(move |_, _| Reply::status(status, "nope"));
      let error = server.rpc(RpcAuth::None).block_count().unwrap_err();
      assert_eq!(server.requests().len(), 1);
      assert!(
        format!("{error:#}").contains(&status.to_string()),
        "{error:#}"
      );
    }
  }

  #[test]
  fn connection_errors_are_retried() {
    let port = {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      listener.local_addr().unwrap().port()
    };
    let rpc = Rpc::with_retry_policy(
      RpcConfig {
        url: format!("127.0.0.1:{port}"),
        auth: RpcAuth::None,
        timeout_secs: 5,
      },
      fast_retries(3),
    )
    .unwrap();
    let error = rpc.block_count().unwrap_err();
    assert!(
      format!("{error:#}").contains("failed after 3 attempts"),
      "{error:#}"
    );
  }

  #[test]
  fn timeouts_are_retried() {
    let server = Server::start(|n, request| Reply {
      delay: if n == 0 {
        Duration::from_millis(2500)
      } else {
        Duration::ZERO
      },
      ..ok(request, "5")
    });
    let rpc = Rpc::with_retry_policy(
      RpcConfig {
        url: server.url.clone(),
        auth: RpcAuth::None,
        timeout_secs: 1,
      },
      fast_retries(3),
    )
    .unwrap();
    let start = Instant::now();
    assert_eq!(rpc.block_count().unwrap(), 5);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(server.requests().len(), 2);
  }

  #[test]
  fn cookie_is_reread_after_401() {
    let dir = tempfile::tempdir().unwrap();
    let cookie = dir.path().join(".cookie");
    fs::write(&cookie, "__cookie__:old").unwrap();

    let good = format!(
      "Basic {}",
      base64::engine::general_purpose::STANDARD.encode("__cookie__:new")
    );
    let server = Server::start(move |_, request| {
      if request.authorization.as_deref() == Some(good.as_str()) {
        ok(request, "1")
      } else {
        Reply::status(401, "")
      }
    });

    let rpc = server.rpc(RpcAuth::Cookie(cookie.clone()));
    fs::write(&cookie, "__cookie__:new\n").unwrap();
    assert_eq!(rpc.block_count().unwrap(), 1);
    assert_eq!(rpc.block_count().unwrap(), 1);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
      requests[0].authorization.as_deref(),
      Some(
        format!(
          "Basic {}",
          base64::engine::general_purpose::STANDARD.encode("__cookie__:old")
        )
        .as_str()
      )
    );
  }

  #[test]
  fn wrong_password_is_not_retried() {
    let server = Server::start(|_, _| Reply::status(401, ""));
    let error = server
      .rpc(RpcAuth::UserPass {
        username: "user".into(),
        password: "wrong".into(),
      })
      .block_count()
      .unwrap_err();
    assert_eq!(server.requests().len(), 1);
    assert!(format!("{error:#}").contains("HTTP 401"), "{error:#}");
    assert_eq!(
      server.requests()[0].authorization.as_deref(),
      Some("Basic dXNlcjp3cm9uZw==")
    );
  }

  #[test]
  fn missing_cookie_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
      Rpc::new(RpcConfig {
        url: "http://127.0.0.1:1".into(),
        auth: RpcAuth::Cookie(dir.path().join(".cookie")),
        timeout_secs: 1,
      })
      .is_err()
    );
  }

  fn batch_ids(request: &Seen) -> Vec<u64> {
    serde_json::from_str::<Vec<Value>>(&request.body)
      .unwrap()
      .iter()
      .map(|request| request["id"].as_u64().unwrap())
      .collect()
  }

  #[test]
  fn batch_results_follow_request_order() {
    let hash = |i: u64| format!("{:064x}", i + 1);
    let server = Server::start(move |_, request| {
      let replies = batch_ids(request)
        .into_iter()
        .rev()
        .map(|id| format!("{{\"result\":\"{}\",\"error\":null,\"id\":{id}}}", hash(id)))
        .collect::<Vec<String>>();
      Reply::json(format!("[{}]", replies.join(",")))
    });
    let rpc = server.rpc(RpcAuth::None);
    let hashes = rpc.block_hashes(10..15).unwrap();
    let ids = batch_ids(&server.requests()[0]);
    assert_eq!(ids.len(), 5);
    let requests = serde_json::from_str::<Vec<Value>>(&server.requests()[0].body).unwrap();
    for (i, (request, hash_value)) in requests.iter().zip(&hashes).enumerate() {
      assert_eq!(request["method"], "getblockhash");
      assert_eq!(request["params"], serde_json::json!([10 + i]));
      assert_eq!(hash_value.to_string(), hash(ids[i]));
    }
    assert!(rpc.block_hashes(3..3).unwrap().is_empty());
  }

  #[test]
  fn batch_errors() {
    let server = Server::start(|n, request| {
      let ids = batch_ids(request);
      let replies = ids
        .iter()
        .enumerate()
        .map(|(i, id)| match (n, i) {
          (0, 1) => format!(
            "{{\"result\":null,\"error\":{{\"code\":-28,\"message\":\"warming up\"}},\"id\":{id}}}"
          ),
          (_, 2) => format!(
            "{{\"result\":null,\"error\":{{\"code\":-8,\"message\":\"Block height out of range\"}},\"id\":{id}}}"
          ),
          _ => format!("{{\"result\":\"{:064x}\",\"error\":null,\"id\":{id}}}", i),
        })
        .collect::<Vec<String>>();
      Reply::json(format!("[{}]", replies.join(",")))
    });
    let error = server.rpc(RpcAuth::None).block_hashes(0..4).unwrap_err();
    // Retried once for -28, then failed on the -8 without retrying.
    assert_eq!(server.requests().len(), 2);
    assert_eq!(error.downcast_ref::<RpcError>().unwrap().code, -8);
    assert!(
      format!("{error:#}").contains("request 2 of the batch"),
      "{error:#}"
    );
  }

  #[test]
  fn block_raw_streams_hex_with_and_without_content_length() {
    let bytes = (0..300_000u32)
      .map(|i| (i % 251) as u8)
      .collect::<Vec<u8>>();
    let hex = ::hex::encode(&bytes);
    for close in [false, true] {
      let hex = hex.clone();
      let server = Server::start(move |_, request| Reply {
        close,
        ..ok(request, &format!("\"{hex}\""))
      });
      let rpc = server.rpc(RpcAuth::None);
      assert_eq!(rpc.block_raw(BlockHash::all_zeros()).unwrap(), bytes);
      let body = serde_json::from_str::<Value>(&server.requests()[0].body).unwrap();
      assert_eq!(body["method"], "getblock");
      assert_eq!(
        body["params"],
        serde_json::json!([BlockHash::all_zeros().to_string(), 0])
      );
    }
  }

  #[test]
  fn block_raw_rejects_bad_responses() {
    for (result, retried) in [("\"0g\"", false), ("123", false), ("\"abc\"", false)] {
      let server = Server::start(move |_, request| ok(request, result));
      assert!(
        server
          .rpc(RpcAuth::None)
          .block_raw(BlockHash::all_zeros())
          .is_err()
      );
      assert_eq!(server.requests().len() > 1, retried, "{result}");
    }

    // Mismatched id.
    let server =
      Server::start(|_, _| Reply::json("{\"result\":\"00\",\"error\":null,\"id\":999999}"));
    let error = server
      .rpc(RpcAuth::None)
      .block_raw(BlockHash::all_zeros())
      .unwrap_err();
    assert!(format!("{error:#}").contains("does not match"), "{error:#}");
  }

  #[test]
  fn block_header_is_checked_against_its_hash() {
    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin);
    let header = ::hex::encode(consensus::serialize(&genesis.header));
    let server = Server::start(move |_, request| ok(request, &format!("\"{header}\"")));
    let rpc = server.rpc(RpcAuth::None);
    assert_eq!(
      rpc.block_header(genesis.block_hash()).unwrap(),
      genesis.header
    );
    assert!(rpc.block_header(BlockHash::all_zeros()).is_err());
  }

  #[test]
  fn url_without_scheme_gets_http() {
    let rpc = Rpc::new(RpcConfig {
      url: "127.0.0.1:8332".into(),
      auth: RpcAuth::None,
      timeout_secs: 0,
    })
    .unwrap();
    assert_eq!(format!("{rpc:?}"), "Rpc { url: \"http://127.0.0.1:8332\" }");
  }
}
