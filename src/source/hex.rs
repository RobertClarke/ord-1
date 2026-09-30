//! Fast hex decoding, and the streaming reader for JSON-RPC responses whose
//! result is one big hex string (`getblock <hash> 0`).

use std::io::{self, Read};

/// ASCII byte → nibble, `INVALID` for anything that is not a hex digit.
static NIBBLE: [u8; 256] = {
  let mut table = [INVALID; 256];
  let mut i = 0;
  while i < 10 {
    table[b'0' as usize + i] = i as u8;
    i += 1;
  }
  let mut i = 0;
  while i < 6 {
    table[b'a' as usize + i] = 10 + i as u8;
    table[b'A' as usize + i] = 10 + i as u8;
    i += 1;
  }
  table
};

const INVALID: u8 = 0xff;

/// Decodes the longest prefix of `src` made of whole hex pairs into `dst`,
/// stopping before the first non-hex byte, at the end of `src` (an odd
/// trailing digit is left undecoded) or when `dst` is full. Returns the number
/// of bytes written; `2 * written` bytes of `src` were consumed.
pub(crate) fn decode_prefix(src: &[u8], dst: &mut [u8]) -> usize {
  let pairs = (src.len() / 2).min(dst.len());
  let mut done = 0;

  // Eight pairs at a time, validity checked once per group.
  while done + 8 <= pairs {
    let s = &src[done * 2..done * 2 + 16];
    let mut bad = 0;
    let mut out = [0u8; 8];
    for (k, byte) in out.iter_mut().enumerate() {
      let hi = NIBBLE[s[2 * k] as usize];
      let lo = NIBBLE[s[2 * k + 1] as usize];
      bad |= hi | lo;
      *byte = (hi << 4) | (lo & 0x0f);
    }
    if bad & 0xf0 != 0 {
      break;
    }
    dst[done..done + 8].copy_from_slice(&out);
    done += 8;
  }

  while done < pairs {
    let hi = NIBBLE[src[done * 2] as usize];
    let lo = NIBBLE[src[done * 2 + 1] as usize];
    if (hi | lo) & 0xf0 != 0 {
      break;
    }
    dst[done] = (hi << 4) | lo;
    done += 1;
  }

  done
}

/// Decodes a whole hex string.
pub(crate) fn decode(src: &[u8]) -> Result<Vec<u8>, HexError> {
  if !src.len().is_multiple_of(2) {
    return Err(HexError::OddLength(src.len()));
  }
  let mut out = vec![0; src.len() / 2];
  let written = decode_prefix(src, &mut out);
  if written != out.len() {
    let position = 2 * written + usize::from(NIBBLE[src[2 * written] as usize] != INVALID);
    return Err(HexError::InvalidCharacter {
      position,
      byte: src[position],
    });
  }
  Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HexError {
  OddLength(usize),
  InvalidCharacter { position: usize, byte: u8 },
}

impl std::fmt::Display for HexError {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    match self {
      Self::OddLength(len) => write!(f, "hex string has odd length {len}"),
      Self::InvalidCharacter { position, byte } => {
        write!(
          f,
          "invalid hex character {byte:#04x} at position {position}"
        )
      }
    }
  }
}

impl std::error::Error for HexError {}

/// Result of reading a JSON-RPC response body that is expected to carry a hex
/// string result.
#[derive(Debug)]
pub(crate) enum HexBody {
  /// The decoded result, plus the rest of the envelope with the string
  /// replaced by `null` (e.g. `{"result":null,"error":null,"id":7}`), for the
  /// caller to check `error` and `id`.
  Hex { bytes: Vec<u8>, envelope: Vec<u8> },
  /// The result is not a string (e.g. an error response): the whole body.
  Other(Vec<u8>),
}

#[derive(Debug)]
pub(crate) enum HexBodyError {
  /// Reading the body failed (connection reset, timeout, truncated body).
  Io(io::Error),
  /// The body is not a well-formed hex string response.
  Malformed(String),
}

impl From<io::Error> for HexBodyError {
  fn from(error: io::Error) -> Self {
    Self::Io(error)
  }
}

/// Longest prefix searched for the start of the result string.
const MAX_HEAD: usize = 64 * 1024;

/// Streams a JSON-RPC response body, hex-decoding the `result` string as it
/// arrives without ever holding the hex text. `size_hint` (the body length,
/// if known) pre-sizes the output.
pub(crate) fn read_hex_result(
  reader: &mut impl Read,
  size_hint: Option<u64>,
) -> Result<HexBody, HexBodyError> {
  let mut buf = vec![0u8; 256 * 1024];
  let mut head = Vec::new();

  // Read until the opening quote of the result string is found.
  let start = loop {
    let n = read_some(reader, &mut buf)?;
    if n == 0 {
      return Ok(HexBody::Other(head));
    }
    head.extend_from_slice(&buf[..n]);
    match find_result_string(&head) {
      Search::Found(start) => break start,
      Search::NotString => {
        reader.read_to_end(&mut head)?;
        return Ok(HexBody::Other(head));
      }
      Search::NeedMore if head.len() > MAX_HEAD => {
        return Err(HexBodyError::Malformed(
          "no \"result\" in the first 64 KiB of the response".into(),
        ));
      }
      Search::NeedMore => {}
    }
  };

  let capacity = size_hint
    .and_then(|len| usize::try_from(len / 2).ok())
    .unwrap_or(head.len())
    .max(64);
  let mut decoder = StreamDecoder {
    out: vec![0; capacity],
    len: 0,
    pending: None,
  };

  let mut envelope = head[..start - 1].to_vec();
  envelope.extend_from_slice(b"null");

  let mut closed = false;
  if let Some(end) = decoder.push(&head[start..])? {
    envelope.extend_from_slice(&head[start + end + 1..]);
    closed = true;
  }
  drop(head);

  while !closed {
    let n = read_some(reader, &mut buf)?;
    if n == 0 {
      return Err(HexBodyError::Io(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "response ended inside the result string",
      )));
    }
    if let Some(end) = decoder.push(&buf[..n])? {
      envelope.extend_from_slice(&buf[end + 1..n]);
      closed = true;
    }
  }

  reader.read_to_end(&mut envelope)?;

  let mut bytes = decoder.out;
  bytes.truncate(decoder.len);
  Ok(HexBody::Hex { bytes, envelope })
}

fn read_some(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
  loop {
    match reader.read(buf) {
      Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
      result => return result,
    }
  }
}

enum Search {
  /// Index of the first byte after the opening quote.
  Found(usize),
  /// `"result"` is followed by something other than a string.
  NotString,
  NeedMore,
}

fn find_result_string(head: &[u8]) -> Search {
  const KEY: &[u8] = b"\"result\"";
  let Some(key) = head.windows(KEY.len()).position(|window| window == KEY) else {
    return Search::NeedMore;
  };
  let mut i = key + KEY.len();
  let mut colon = false;
  while i < head.len() {
    match head[i] {
      b' ' | b'\t' | b'\r' | b'\n' => {}
      b':' if !colon => colon = true,
      b'"' if colon => return Search::Found(i + 1),
      _ => return Search::NotString,
    }
    i += 1;
  }
  Search::NeedMore
}

struct StreamDecoder {
  out: Vec<u8>,
  len: usize,
  /// High nibble of a pair split across chunks.
  pending: Option<u8>,
}

impl StreamDecoder {
  /// Decodes `chunk`; returns the index of the closing quote once found.
  fn push(&mut self, chunk: &[u8]) -> Result<Option<usize>, HexBodyError> {
    let mut i = 0;

    if let Some(hi) = self.pending {
      let Some(&c) = chunk.first() else {
        return Ok(None);
      };
      if c == b'"' {
        return Err(Self::odd());
      }
      let lo = NIBBLE[c as usize];
      if lo == INVALID {
        return Err(Self::invalid(c));
      }
      self.reserve(1);
      self.out[self.len] = (hi << 4) | lo;
      self.len += 1;
      self.pending = None;
      i = 1;
    }

    loop {
      let rest = &chunk[i..];
      self.reserve(rest.len() / 2 + 1);
      let written = decode_prefix(rest, &mut self.out[self.len..]);
      self.len += written;
      i += 2 * written;

      match chunk.len() - i {
        0 => return Ok(None),
        1 => {
          let c = chunk[i];
          if c == b'"' {
            return Ok(Some(i));
          }
          let hi = NIBBLE[c as usize];
          if hi == INVALID {
            return Err(Self::invalid(c));
          }
          self.pending = Some(hi);
          return Ok(None);
        }
        _ => {
          let (a, b) = (chunk[i], chunk[i + 1]);
          if a == b'"' {
            return Ok(Some(i));
          }
          if NIBBLE[a as usize] == INVALID {
            return Err(Self::invalid(a));
          }
          if b == b'"' {
            return Err(Self::odd());
          }
          if NIBBLE[b as usize] == INVALID {
            return Err(Self::invalid(b));
          }
          // `dst` was full: grow and continue.
        }
      }
    }
  }

  fn reserve(&mut self, additional: usize) {
    let needed = self.len + additional;
    if needed > self.out.len() {
      self.out.resize(needed.max(self.out.len() * 2), 0);
    }
  }

  fn odd() -> HexBodyError {
    HexBodyError::Malformed("result string has an odd number of hex digits".into())
  }

  fn invalid(byte: u8) -> HexBodyError {
    HexBodyError::Malformed(format!(
      "invalid character {byte:#04x} in result hex string"
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn decode_valid() {
    assert_eq!(decode(b"").unwrap(), Vec::<u8>::new());
    assert_eq!(decode(b"00ff10aB").unwrap(), vec![0x00, 0xff, 0x10, 0xab]);
    let bytes = (0..=255u8).collect::<Vec<u8>>();
    assert_eq!(decode(::hex::encode(&bytes).as_bytes()).unwrap(), bytes);
    assert_eq!(
      decode(::hex::encode_upper(&bytes).as_bytes()).unwrap(),
      bytes
    );
  }

  #[test]
  fn decode_invalid() {
    assert_eq!(decode(b"abc"), Err(HexError::OddLength(3)));
    assert_eq!(
      decode(b"0g"),
      Err(HexError::InvalidCharacter {
        position: 1,
        byte: b'g'
      })
    );
    assert_eq!(
      decode(b"00112233445566778899aabbccddeeff00x1"),
      Err(HexError::InvalidCharacter {
        position: 34,
        byte: b'x'
      })
    );
    for bad in [b'"', b' ', b'G', b'z', b'/', b':', b'@', b'`', 0x80, 0xff] {
      let mut text = b"0123456789abcdef0123456789abcdef".to_vec();
      text[21] = bad;
      assert_eq!(
        decode(&text),
        Err(HexError::InvalidCharacter {
          position: 21,
          byte: bad
        })
      );
    }
  }

  #[test]
  fn decode_prefix_stops_at_non_hex_and_full_destination() {
    let mut dst = [0u8; 64];
    assert_eq!(decode_prefix(b"0011223344556677889\"", &mut dst), 9);
    assert_eq!(decode_prefix(b"00112233445566778899\"", &mut dst), 10);
    assert_eq!(decode_prefix(b"001", &mut dst), 1);
    let mut small = [0u8; 3];
    assert_eq!(decode_prefix(b"0011223344", &mut small), 3);
    assert_eq!(small, [0x00, 0x11, 0x22]);
  }

  /// Returns at most `step` bytes per read.
  struct Trickle<'a> {
    data: &'a [u8],
    step: usize,
  }

  impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
      let n = self.step.min(buf.len()).min(self.data.len());
      buf[..n].copy_from_slice(&self.data[..n]);
      self.data = &self.data[n..];
      Ok(n)
    }
  }

  fn read_all(body: &[u8], step: usize, hint: Option<u64>) -> Result<HexBody, HexBodyError> {
    read_hex_result(&mut Trickle { data: body, step }, hint)
  }

  #[test]
  fn streams_hex_result_across_any_chunking() {
    let payload = (0..3000u32)
      .map(|i| (i * 7 % 256) as u8)
      .collect::<Vec<u8>>();
    let body = format!(
      "{{\"result\":\"{}\",\"error\":null,\"id\":42}}\n",
      ::hex::encode(&payload)
    );
    for step in [1, 2, 3, 5, 7, 16, 17, 1000, 4096, 1 << 20] {
      for hint in [None, Some(body.len() as u64), Some(10)] {
        match read_all(body.as_bytes(), step, hint).unwrap() {
          HexBody::Hex { bytes, envelope } => {
            assert_eq!(bytes, payload, "step {step}");
            assert_eq!(
              String::from_utf8(envelope).unwrap(),
              "{\"result\":null,\"error\":null,\"id\":42}\n"
            );
          }
          other => panic!("unexpected {other:?}"),
        }
      }
    }
  }

  #[test]
  fn accepts_other_envelope_layouts() {
    for body in [
      "{\"jsonrpc\":\"2.0\",\"result\":\"00ff\",\"id\":1}",
      "{ \"result\" : \"00ff\" , \"error\" : null , \"id\" : 1 }",
      "{\"id\":1,\"error\":null,\"result\":\"00ff\"}",
    ] {
      for step in [1, 3, 100] {
        match read_all(body.as_bytes(), step, None).unwrap() {
          HexBody::Hex { bytes, envelope } => {
            assert_eq!(bytes, [0x00, 0xff]);
            let value: serde_json::Value = serde_json::from_slice(&envelope).unwrap();
            assert_eq!(value["result"], serde_json::Value::Null);
            assert_eq!(value["id"], 1);
          }
          other => panic!("unexpected {other:?}"),
        }
      }
    }
  }

  #[test]
  fn empty_result_string() {
    match read_all(b"{\"result\":\"\",\"error\":null,\"id\":1}", 4, None).unwrap() {
      HexBody::Hex { bytes, .. } => assert!(bytes.is_empty()),
      other => panic!("unexpected {other:?}"),
    }
  }

  #[test]
  fn non_string_results_are_returned_whole() {
    let body = "{\"result\":null,\"error\":{\"code\":-5,\"message\":\"Block not found\"},\"id\":1}";
    for step in [1, 2, 1000] {
      match read_all(body.as_bytes(), step, None).unwrap() {
        HexBody::Other(bytes) => assert_eq!(bytes, body.as_bytes()),
        other => panic!("unexpected {other:?}"),
      }
    }
    match read_all(b"", 10, None).unwrap() {
      HexBody::Other(bytes) => assert!(bytes.is_empty()),
      other => panic!("unexpected {other:?}"),
    }
  }

  #[test]
  fn malformed_hex_results() {
    for (body, step) in [
      ("{\"result\":\"00f\",\"id\":1}", 1),
      ("{\"result\":\"00f\",\"id\":1}", 100),
      ("{\"result\":\"00fg\",\"id\":1}", 1),
      ("{\"result\":\"00fg\",\"id\":1}", 100),
      ("{\"result\":\"0 ff\",\"id\":1}", 2),
    ] {
      assert!(
        matches!(
          read_all(body.as_bytes(), step, None),
          Err(HexBodyError::Malformed(_))
        ),
        "{body} {step}"
      );
    }
  }

  #[test]
  fn truncated_body_is_an_io_error() {
    assert!(matches!(
      read_all(b"{\"result\":\"00ff00", 3, None),
      Err(HexBodyError::Io(_))
    ));
  }
}
