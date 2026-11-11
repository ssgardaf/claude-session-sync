//! A JSON reader that draws the line between "JSON" and "not JSON" where the Python version
//! of this tool drew it when it read session records (Python's `json.loads` after a
//! `utf-8-sig` decode).
//!
//! Which files count as a readable record, a broken write ("torn") or something foreign
//! decides what may be replaced, so the reader must not be stricter or laxer than before:
//! a lone `\uD800` escape is a character (kept here as U+FFFD), duplicate keys are allowed
//! (the last one wins), and an integer longer than 4300 digits is an error.
//!
//! One thing is deliberately different. Python also reads `NaN`, `Infinity` and `-Infinity`
//! as numbers; the desktop app (`JSON.parse`) does not. `parse` still reads them, so that
//! such a file is not mistaken for a broken write, but says so, and `loads` refuses the
//! document with `JsonError::NonFinite`: a record the app cannot read is left where it is
//! and never handed to other accounts.

/// A parsed JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    /// An integer, kept as its digits (it may be larger than any machine number).
    Int(String),
    /// A number with a fraction or exponent, or `NaN` / `Infinity` / `-Infinity`.
    Float(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonError {
    /// Not JSON.
    Syntax,
    /// Nested deeper than the reader follows.
    TooDeep,
    /// Well formed but for a `NaN`, `Infinity` or `-Infinity`, which the app cannot read.
    NonFinite,
}

/// An integer too large to be a finite number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow;

const MAX_DEPTH: usize = 900;
const MAX_INT_DIGITS: usize = 4300;

impl J {
    /// The value of `key` in an object (the last one, if the key is repeated).
    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(items) => items.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, J::Obj(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[J]> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// A finite number, or None for anything else (booleans are not numbers).
    pub fn num(&self) -> Result<Option<f64>, Overflow> {
        match self {
            J::Int(digits) => match digits.parse::<f64>() {
                Ok(v) if v.is_finite() => Ok(Some(v)),
                _ => Err(Overflow),
            },
            J::Float(v) if v.is_finite() => Ok(Some(*v)),
            _ => Ok(None),
        }
    }

    /// False for null, false, zero, and anything empty.
    pub fn truthy(&self) -> bool {
        match self {
            J::Null => false,
            J::Bool(b) => *b,
            J::Int(d) => d.bytes().any(|b| b.is_ascii_digit() && b != b'0'),
            J::Float(v) => *v != 0.0,
            J::Str(s) => !s.is_empty(),
            J::Arr(a) => !a.is_empty(),
            J::Obj(o) => !o.is_empty(),
        }
    }

    /// The value as text (a string as it is; other values in a plain written form).
    pub fn text(&self) -> String {
        match self {
            J::Null => String::from("None"),
            J::Bool(true) => String::from("True"),
            J::Bool(false) => String::from("False"),
            J::Int(d) => d.clone(),
            J::Float(v) if v.is_nan() => String::from("nan"),
            J::Float(v) if v.is_infinite() => String::from(if *v > 0.0 { "inf" } else { "-inf" }),
            J::Float(v) => format!("{:?}", v),
            J::Str(s) => s.clone(),
            J::Arr(a) => format!("[{}]", a.iter().map(J::repr).collect::<Vec<_>>().join(", ")),
            J::Obj(o) => format!(
                "{{{}}}",
                o.iter()
                    .map(|(k, v)| format!("'{}': {}", k, v.repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn repr(&self) -> String {
        match self {
            J::Str(s) => format!("'{}'", s),
            other => other.text(),
        }
    }
}

/// Decode UTF-8, dropping one leading byte-order mark. None if the bytes are not UTF-8.
pub fn decode_utf8_sig(bytes: &[u8]) -> Option<&str> {
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    std::str::from_utf8(body).ok()
}

/// Parse a complete document as Python does. The flag says whether a `NaN`, `Infinity` or
/// `-Infinity` was among the values.
pub fn parse(text: &str) -> Result<(J, bool), JsonError> {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
        non_finite: false,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(JsonError::Syntax);
    }
    Ok((v, p.non_finite))
}

/// Parse a complete JSON document that the app can read too: one that is well formed apart
/// from a `NaN`, `Infinity` or `-Infinity` is refused as `JsonError::NonFinite`.
pub fn loads(text: &str) -> Result<J, JsonError> {
    match parse(text)? {
        (_, true) => Err(JsonError::NonFinite),
        (v, false) => Ok(v),
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    non_finite: bool,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, word: &[u8]) -> bool {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<J, JsonError> {
        if depth > MAX_DEPTH {
            return Err(JsonError::TooDeep);
        }
        match self.b.get(self.i).copied() {
            Some(b'"') => {
                self.i += 1;
                self.string().map(J::Str)
            }
            Some(b'{') => {
                self.i += 1;
                self.object(depth)
            }
            Some(b'[') => {
                self.i += 1;
                self.array(depth)
            }
            Some(b'n') if self.eat(b"null") => Ok(J::Null),
            Some(b't') if self.eat(b"true") => Ok(J::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(J::Bool(false)),
            Some(b'N') if self.eat(b"NaN") => self.non_finite(f64::NAN),
            Some(b'I') if self.eat(b"Infinity") => self.non_finite(f64::INFINITY),
            Some(b'-') if self.eat(b"-Infinity") => self.non_finite(f64::NEG_INFINITY),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(JsonError::Syntax),
        }
    }

    fn non_finite(&mut self, v: f64) -> Result<J, JsonError> {
        self.non_finite = true;
        Ok(J::Float(v))
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while self.b.get(self.i).is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Result<J, JsonError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.b.get(self.i).copied() {
            Some(b'0') => self.i += 1,
            Some(c) if c.is_ascii_digit() => {
                self.digits();
            }
            _ => return Err(JsonError::Syntax),
        }
        let int_end = self.i;
        let mut is_float = false;
        if self.b.get(self.i) == Some(&b'.')
            && self.b.get(self.i + 1).is_some_and(|c| c.is_ascii_digit())
        {
            self.i += 1;
            self.digits();
            is_float = true;
        }
        if matches!(self.b.get(self.i), Some(b'e') | Some(b'E')) {
            let mark = self.i;
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if self.digits() > 0 {
                is_float = true;
            } else {
                self.i = mark;
            }
        }
        // the slice holds ASCII only
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| JsonError::Syntax)?;
        if is_float {
            return text
                .parse::<f64>()
                .map(J::Float)
                .map_err(|_| JsonError::Syntax);
        }
        let digits = text.trim_start_matches('-');
        if digits.len() > MAX_INT_DIGITS {
            return Err(JsonError::Syntax);
        }
        debug_assert_eq!(int_end, self.i);
        if digits == "0" {
            return Ok(J::Int(String::from("0")));
        }
        Ok(J::Int(text.to_string()))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let h = self.b.get(self.i..self.i + 4).ok_or(JsonError::Syntax)?;
        let mut v = 0u32;
        for &c in h {
            v = v * 16 + (c as char).to_digit(16).ok_or(JsonError::Syntax)?;
        }
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, JsonError> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i).ok_or(JsonError::Syntax)?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or(JsonError::Syntax)?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let mut u = self.hex4()?;
                            if (0xD800..0xDC00).contains(&u) && self.b[self.i..].starts_with(b"\\u")
                            {
                                let mark = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    u = 0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    self.i = mark;
                                }
                            }
                            char::from_u32(u).unwrap_or('\u{FFFD}')
                        }
                        _ => return Err(JsonError::Syntax),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c if c < 0x20 => return Err(JsonError::Syntax),
                c => out.push(c),
            }
        }
        // the input was valid UTF-8 and only whole characters were copied
        String::from_utf8(out).map_err(|_| JsonError::Syntax)
    }

    fn object(&mut self, depth: usize) -> Result<J, JsonError> {
        let mut items = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(J::Obj(items));
        }
        loop {
            if self.b.get(self.i) != Some(&b'"') {
                return Err(JsonError::Syntax);
            }
            self.i += 1;
            let key = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(JsonError::Syntax);
            }
            self.i += 1;
            self.ws();
            let v = self.value(depth + 1)?;
            items.push((key, v));
            self.ws();
            match self.b.get(self.i) {
                Some(b'}') => {
                    self.i += 1;
                    return Ok(J::Obj(items));
                }
                Some(b',') => {
                    self.i += 1;
                    self.ws();
                }
                _ => return Err(JsonError::Syntax),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<J, JsonError> {
        let mut items = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(J::Arr(items));
        }
        loop {
            let v = self.value(depth + 1)?;
            items.push(v);
            self.ws();
            match self.b.get(self.i) {
                Some(b']') => {
                    self.i += 1;
                    return Ok(J::Arr(items));
                }
                Some(b',') => {
                    self.i += 1;
                    self.ws();
                }
                _ => return Err(JsonError::Syntax),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_documents() {
        let j = loads(r#" {"a": 1, "b": [true, false, null, "x\n\u00e9"], "c": -2.5e3, "a": 2} "#)
            .unwrap();
        assert_eq!(j.get("a"), Some(&J::Int("2".into())));
        assert_eq!(j.get("c").unwrap().num(), Ok(Some(-2500.0)));
        assert_eq!(
            j.get("b").unwrap().as_array().unwrap()[3],
            J::Str("x\né".into())
        );
        assert_eq!(loads("[]"), Ok(J::Arr(vec![])));
        assert_eq!(loads("{}"), Ok(J::Obj(vec![])));
    }

    #[test]
    fn what_python_accepts_beyond_strict_json() {
        let j = loads(r#"{"d":"\ud83d","e":"\ud83d\ude00"}"#).unwrap();
        assert_eq!(j.get("d").unwrap().as_str(), Some("\u{FFFD}"));
        assert_eq!(j.get("e").unwrap().as_str(), Some("\u{1F600}"));
        assert_eq!(loads("1e999").unwrap().num(), Ok(None)); // (JSON, also to the app: it reads Infinity)
    }

    #[test]
    fn numbers_the_app_cannot_read_are_told_apart_from_broken_files() {
        for text in [
            r#"{"a":NaN}"#,
            "[1, Infinity]",
            "-Infinity",
            r#"{"a":{"b":[NaN]}}"#,
        ] {
            assert_eq!(loads(text), Err(JsonError::NonFinite), "{}", text);
            assert!(parse(text).unwrap().1, "{}", text);
        }
        let (j, flagged) = parse(r#"{"a":NaN,"b":Infinity,"c":-Infinity,"d":1}"#).unwrap();
        assert!(flagged);
        for key in ["a", "b", "c"] {
            assert_eq!(j.get(key).unwrap().num(), Ok(None));
        }
        assert!(!parse(r#"{"a":1}"#).unwrap().1);
        // inside a string these are just text
        assert_eq!(
            loads(r#"{"title":"NaN Infinity"}"#)
                .unwrap()
                .get("title")
                .unwrap()
                .as_str(),
            Some("NaN Infinity")
        );
        // a file that is broken as well is broken first
        assert_eq!(loads(r#"{"a":NaN,"#), Err(JsonError::Syntax));
        assert_eq!(loads("NaN x"), Err(JsonError::Syntax));
        assert_eq!(loads("-Infinit"), Err(JsonError::Syntax));
    }

    #[test]
    fn what_is_not_json() {
        for bad in [
            "",
            " ",
            "{",
            "[1,]",
            "{\"a\":1,}",
            "{a:1}",
            "01",
            "1.",
            "-",
            "\"a",
            "\"\t\"",
            "\"\\x\"",
            "nul",
            "[1] x",
            "{\"a\" 1}",
            "'a'",
            "\u{feff}{}",
            "\"\\u12g4\"",
            "+1",
            ".5",
        ] {
            assert_eq!(loads(bad), Err(JsonError::Syntax), "{:?}", bad);
        }
        assert_eq!(loads(&"9".repeat(4301)), Err(JsonError::Syntax));
        assert_eq!(loads(&"9".repeat(4300)).unwrap().num(), Err(Overflow));
        let deep = "[".repeat(5000) + &"]".repeat(5000);
        assert_eq!(loads(&deep), Err(JsonError::TooDeep));
    }

    #[test]
    fn byte_order_mark_and_bad_utf8() {
        assert_eq!(decode_utf8_sig(b"\xef\xbb\xbf{}"), Some("{}"));
        assert_eq!(decode_utf8_sig(b"{}"), Some("{}"));
        assert_eq!(decode_utf8_sig(b"\xff{}"), None);
    }

    #[test]
    fn truthiness_and_text() {
        assert!(!loads("0").unwrap().truthy());
        assert!(!loads("-0").unwrap().truthy());
        assert!(!loads("0.0").unwrap().truthy());
        assert!(!loads("\"\"").unwrap().truthy());
        assert!(!loads("[]").unwrap().truthy());
        assert!(parse("NaN").unwrap().0.truthy());
        assert!(loads("12345").unwrap().truthy());
        assert_eq!(loads("12345").unwrap().text(), "12345");
        assert_eq!(loads("true").unwrap().text(), "True");
    }
}
