//! Parser for Valve KeyValues text (VDF / ACF).

use std::fmt;

const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vdf {
    Str(String),
    Obj(Vec<(String, Vdf)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VdfError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for VdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for VdfError {}

impl Vdf {
    /// First child with the given key (ASCII case-insensitive). `None` for strings.
    pub fn get(&self, key: &str) -> Option<&Vdf> {
        match self {
            Vdf::Obj(v) => v
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Vdf::Str(_) => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Vdf::Str(s) => Some(s),
            Vdf::Obj(_) => None,
        }
    }

    /// Iterate over (key, value) pairs; empty for strings.
    pub fn iter_obj(&self) -> impl Iterator<Item = (&str, &Vdf)> {
        let slice: &[(String, Vdf)] = match self {
            Vdf::Obj(v) => v,
            Vdf::Str(_) => &[],
        };
        slice.iter().map(|(k, v)| (k.as_str(), v))
    }
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
    line: usize,
}

impl Parser<'_> {
    fn err<T>(&self, msg: &str) -> Result<T, VdfError> {
        Err(VdfError {
            line: self.line,
            msg: msg.to_string(),
        })
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b'\n' {
                self.line += 1;
                self.pos += 1;
            } else if c.is_ascii_whitespace() {
                self.pos += 1;
            } else if c == b'/' && self.b.get(self.pos + 1) == Some(&b'/') {
                while let Some(c) = self.peek() {
                    if c == b'\n' {
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn token(&mut self) -> Result<String, VdfError> {
        if self.peek() == Some(b'"') {
            self.pos += 1;
            let start_line = self.line;
            let mut out = Vec::new();
            loop {
                let Some(c) = self.peek() else {
                    return Err(VdfError {
                        line: start_line,
                        msg: "unterminated string".into(),
                    });
                };
                self.pos += 1;
                match c {
                    b'"' => break,
                    b'\n' => {
                        self.line += 1;
                        out.push(c);
                    }
                    b'\\' => match self.peek() {
                        Some(b'\\') => {
                            out.push(b'\\');
                            self.pos += 1;
                        }
                        Some(b'"') => {
                            out.push(b'"');
                            self.pos += 1;
                        }
                        Some(b'n') => {
                            out.push(b'\n');
                            self.pos += 1;
                        }
                        Some(b't') => {
                            out.push(b'\t');
                            self.pos += 1;
                        }
                        _ => out.push(b'\\'),
                    },
                    _ => out.push(c),
                }
            }
            Ok(String::from_utf8_lossy(&out).into_owned())
        } else {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_whitespace() || matches!(c, b'"' | b'{' | b'}') {
                    break;
                }
                self.pos += 1;
            }
            if start == self.pos {
                return self.err("expected token");
            }
            Ok(String::from_utf8_lossy(&self.b[start..self.pos]).into_owned())
        }
    }

    /// Skip an optional `[$COND]` tag.
    fn skip_cond(&mut self) -> Result<(), VdfError> {
        self.skip_ws();
        if self.peek() == Some(b'[') {
            while let Some(c) = self.peek() {
                if c == b'\n' {
                    return self.err("unterminated conditional");
                }
                self.pos += 1;
                if c == b']' {
                    return Ok(());
                }
            }
            return self.err("unterminated conditional");
        }
        Ok(())
    }

    fn object(&mut self, depth: usize) -> Result<Vec<(String, Vdf)>, VdfError> {
        if depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    if depth == 0 {
                        return Ok(items);
                    }
                    return self.err("unexpected end of input, missing '}'");
                }
                Some(b'}') => {
                    if depth == 0 {
                        return self.err("unexpected '}'");
                    }
                    self.pos += 1;
                    return Ok(items);
                }
                Some(b'{') => return self.err("unexpected '{'"),
                _ => {}
            }
            let key = self.token()?;
            self.skip_cond()?;
            self.skip_ws();
            match self.peek() {
                Some(b'{') => {
                    self.pos += 1;
                    let sub = self.object(depth + 1)?;
                    items.push((key, Vdf::Obj(sub)));
                }
                Some(b'}') | None => return self.err("missing value for key"),
                _ => {
                    let val = self.token()?;
                    self.skip_cond()?;
                    items.push((key, Vdf::Str(val)));
                }
            }
        }
    }
}

/// Parse VDF text; the result is always `Vdf::Obj` (the implicit root).
pub fn parse(text: &str) -> Result<Vdf, VdfError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = Parser {
        b: text.as_bytes(),
        pos: 0,
        line: 1,
    };
    Ok(Vdf::Obj(p.object(0)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_nested_and_order() {
        let v = parse("\"a\" { \"x\" \"1\" \"y\" { \"z\" \"2\" } } \"a\" \"dup\"").unwrap();
        let items: Vec<_> = v.iter_obj().collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].1.as_str(), Some("dup"));
        assert_eq!(
            v.get("A")
                .unwrap()
                .get("Y")
                .unwrap()
                .get("z")
                .unwrap()
                .as_str(),
            Some("2")
        );
    }

    #[test]
    fn unquoted_comments_conditionals_bom_crlf() {
        let t = "\u{feff}// c\r\nroot\r\n{\r\n  key value [$WIN32]\r\n  \"k2\" \"v2\" // trailing\r\n  sub [$X360] { a b }\r\n}\r\n";
        let v = parse(t).unwrap();
        let r = v.get("root").unwrap();
        assert_eq!(r.get("key").unwrap().as_str(), Some("value"));
        assert_eq!(r.get("k2").unwrap().as_str(), Some("v2"));
        assert_eq!(r.get("sub").unwrap().get("a").unwrap().as_str(), Some("b"));
    }

    #[test]
    fn escapes() {
        let v = parse(r#""p" "D:\\Lib\"x\ny\tz""#).unwrap();
        assert_eq!(v.get("p").unwrap().as_str(), Some("D:\\Lib\"x\ny\tz"));
    }

    #[test]
    fn errors_have_line_numbers() {
        let e = parse("\"a\"\n{\n\"b\" \"c\"\n").unwrap_err();
        assert_eq!(e.line, 4);
        let e = parse("\"a\" \"b\"\n}\n").unwrap_err();
        assert_eq!(e.line, 2);
        let e = parse("\"a\" \"b\n\nc").unwrap_err();
        assert_eq!(e.line, 1);
        let e = parse("\"a\"\n").unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn nesting_cap() {
        let ok = "\"k\" {".repeat(MAX_DEPTH) + &"}".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok());
        let bad = "\"k\" {".repeat(MAX_DEPTH + 2) + &"}".repeat(MAX_DEPTH + 2);
        assert!(parse(&bad).unwrap_err().msg.contains("deep"));
    }
}
