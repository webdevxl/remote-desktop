//! Bencoding, the BitTorrent DHT's wire format (BEP 3): integers `i42e`, byte strings `4:spam`,
//! lists `l...e` and dictionaries `d...e` with byte-string keys in sorted order.
//!
//! Decoding is strict about structure (the whole input is one value, nesting is bounded, numbers
//! have no leading zeros or `-0`) but accepts dictionary keys out of order, as some DHT nodes send
//! them. Encoding always sorts keys (a `BTreeMap`), which BEP 44 signatures depend on.

use std::collections::BTreeMap;

/// Deeper nesting than any DHT message has; bounds the recursion on hostile input.
const MAX_DEPTH: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn bytes(b: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(b.into())
    }

    /// The entry `key` of a dictionary.
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        match self {
            Self::Dict(d) => d.get(key),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Self::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Self::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// `get(key)` as a byte string.
    pub fn bytes_at(&self, key: &[u8]) -> Option<&[u8]> {
        self.get(key)?.as_bytes()
    }

    /// `get(key)` as an integer.
    pub fn int_at(&self, key: &[u8]) -> Option<i64> {
        self.get(key)?.as_int()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            Self::Bytes(b) => encode_bytes(b, out),
            Self::List(l) => {
                out.push(b'l');
                l.iter().for_each(|v| v.encode_into(out));
                out.push(b'e');
            }
            Self::Dict(d) => {
                out.push(b'd');
                for (k, v) in d {
                    encode_bytes(k, out);
                    v.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }
}

/// A dictionary from `(key, value)` pairs.
pub fn dict<const N: usize>(entries: [(&[u8], Value); N]) -> Value {
    Value::Dict(entries.into_iter().map(|(k, v)| (k.to_vec(), v)).collect())
}

pub fn encode_bytes(b: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(b);
}

/// The value `input` holds, all of it; None if it is malformed or has bytes left over.
pub fn decode(input: &[u8]) -> Option<Value> {
    let mut parser = Parser { input, pos: 0 };
    let value = parser.value(0)?;
    (parser.pos == input.len()).then_some(value)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        match *self.input.get(self.pos)? {
            b'i' => {
                self.pos += 1;
                let n = self.int_until(b'e')?;
                Some(Value::Int(n))
            }
            b'l' => {
                self.pos += 1;
                let mut list = Vec::new();
                while *self.input.get(self.pos)? != b'e' {
                    list.push(self.value(depth + 1)?);
                }
                self.pos += 1;
                Some(Value::List(list))
            }
            b'd' => {
                self.pos += 1;
                let mut dict = BTreeMap::new();
                while *self.input.get(self.pos)? != b'e' {
                    let key = self.byte_string()?.to_vec();
                    let value = self.value(depth + 1)?;
                    dict.insert(key, value);
                }
                self.pos += 1;
                Some(Value::Dict(dict))
            }
            b'0'..=b'9' => self.byte_string().map(|b| Value::Bytes(b.to_vec())),
            _ => None,
        }
    }

    fn byte_string(&mut self) -> Option<&[u8]> {
        let len = usize::try_from(self.int_until(b':')?).ok()?;
        let end = self.pos.checked_add(len)?;
        let bytes = self.input.get(self.pos..end)?;
        self.pos = end;
        Some(bytes)
    }

    /// A decimal integer up to `end`, which it consumes.
    fn int_until(&mut self, end: u8) -> Option<i64> {
        let rest = &self.input[self.pos..];
        let len = rest.iter().position(|&b| b == end)?;
        let digits = std::str::from_utf8(&rest[..len]).ok()?;
        let unsigned = digits.strip_prefix('-').unwrap_or(digits);
        let canonical = !unsigned.is_empty()
            && unsigned.bytes().all(|b| b.is_ascii_digit())
            && (unsigned == "0" || !unsigned.starts_with('0'))
            && digits != "-0";
        if !canonical {
            return None;
        }
        let n = digits.parse().ok()?;
        self.pos += len + 1;
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let v = dict([
            (b"t", Value::bytes(*b"aa")),
            (b"y", Value::bytes(*b"q")),
            (b"a", dict([(b"id", Value::bytes([7u8; 20])), (b"seq", Value::Int(-3))])),
            (b"l", Value::List(vec![Value::Int(0), Value::bytes(Vec::new())])),
        ]);
        let wire = v.encode();
        assert_eq!(decode(&wire), Some(v));
    }

    #[test]
    fn encodes_like_the_spec() {
        assert_eq!(Value::Int(42).encode(), b"i42e");
        assert_eq!(Value::bytes(*b"spam").encode(), b"4:spam");
        assert_eq!(dict([(b"spam", Value::bytes(*b"eggs")), (b"cow", Value::bytes(*b"moo"))]).encode(), b"d3:cow3:moo4:spam4:eggse");
    }

    #[test]
    fn accepts_keys_out_of_order() {
        let v = decode(b"d1:yi1e1:ai2ee").unwrap();
        assert_eq!(v.int_at(b"a"), Some(2));
        assert_eq!(v.int_at(b"y"), Some(1));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            &b""[..],
            b"i01e",
            b"i-0e",
            b"ie",
            b"i1",
            b"5:abc",
            b"d1:ae",
            b"l",
            b"i1ei2e",
            b"x",
            b"d3:keyi1e",
            b"-1:a",
            b"99999999999999999999999:a",
        ] {
            assert_eq!(decode(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn bounds_nesting() {
        let deep = [vec![b'l'; 100], vec![b'e'; 100]].concat();
        assert_eq!(decode(&deep), None);
        let ok = [vec![b'l'; 10], vec![b'e'; 10]].concat();
        assert!(decode(&ok).is_some());
    }
}
