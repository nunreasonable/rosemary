//! A bounded subset of the documented requirement language. Unknown terms fail.
use der::{Decode, Tagged};

const INVALID: i32 = -67052;
const UNSUPPORTED: i32 = -67051;

#[derive(Debug)]
pub(super) enum Expr {
    Bool(bool),
    Identifier(String),
    AppleGeneric,
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    CertExtension(usize, String),
    LeafOu(String),
}

impl Expr {
    pub fn matches(
        &self,
        identifier: &str,
        apple_trusted: bool,
        certificates: &[Vec<u8>],
    ) -> Result<bool, i32> {
        Ok(match self {
            Self::Bool(b) => *b,
            Self::Identifier(expected) => expected == identifier,
            Self::AppleGeneric => apple_trusted,
            Self::And(a, b) => {
                a.matches(identifier, apple_trusted, certificates)?
                    & b.matches(identifier, apple_trusted, certificates)?
            }
            Self::Or(a, b) => {
                a.matches(identifier, apple_trusted, certificates)?
                    | b.matches(identifier, apple_trusted, certificates)?
            }
            Self::Not(a) => !a.matches(identifier, apple_trusted, certificates)?,
            Self::CertExtension(index, oid) => {
                let Some(bytes) = certificates.get(*index) else {
                    return Ok(false);
                };
                let cert = x509_cert::Certificate::from_der(bytes).map_err(|_| INVALID)?;
                cert.tbs_certificate
                    .extensions
                    .as_ref()
                    .is_some_and(|extensions| {
                        extensions.iter().any(|ext| ext.extn_id.to_string() == *oid)
                    })
            }
            Self::LeafOu(expected) => {
                let Some(bytes) = certificates.first() else {
                    return Ok(false);
                };
                let cert = x509_cert::Certificate::from_der(bytes).map_err(|_| INVALID)?;
                let mut found = false;
                for rdn in cert.tbs_certificate.subject.0.iter() {
                    for attribute in rdn.0.iter() {
                        if attribute.oid.to_string() == "2.5.4.11" {
                            let value = attribute.value.value();
                            if !matches!(
                                attribute.value.tag(),
                                der::Tag::Utf8String | der::Tag::PrintableString
                            ) {
                                return Err(UNSUPPORTED);
                            }
                            found |= value == expected.as_bytes();
                        }
                    }
                }
                found
            }
        })
    }
}

struct Parser {
    tokens: Vec<String>,
    cursor: usize,
}
impl Parser {
    fn take(&mut self) -> Result<String, i32> {
        let token = self.tokens.get(self.cursor).ok_or(INVALID)?.clone();
        self.cursor += 1;
        Ok(token)
    }
    fn accept(&mut self, value: &str) -> bool {
        if self.tokens.get(self.cursor).is_some_and(|s| s == value) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, value: &str) -> Result<(), i32> {
        if self.accept(value) {
            Ok(())
        } else {
            Err(INVALID)
        }
    }
    fn expression(&mut self, depth: usize) -> Result<Expr, i32> {
        if depth > 32 {
            return Err(INVALID);
        }
        let mut expr = self.conjunction(depth + 1)?;
        while self.accept("or") {
            expr = Expr::Or(Box::new(expr), Box::new(self.conjunction(depth + 1)?));
        }
        Ok(expr)
    }
    fn conjunction(&mut self, depth: usize) -> Result<Expr, i32> {
        let mut expr = self.atom(depth + 1)?;
        while self.accept("and") {
            expr = Expr::And(Box::new(expr), Box::new(self.atom(depth + 1)?));
        }
        Ok(expr)
    }
    fn atom(&mut self, depth: usize) -> Result<Expr, i32> {
        if depth > 32 {
            return Err(INVALID);
        }
        Ok(match self.take()?.as_str() {
            "true" => Expr::Bool(true),
            "false" => Expr::Bool(false),
            "identifier" => Expr::Identifier(self.take()?),
            "anchor" => {
                self.expect("apple")?;
                self.expect("generic")?;
                Expr::AppleGeneric
            }
            "not" | "!" => Expr::Not(Box::new(self.atom(depth + 1)?)),
            "(" => {
                let expr = self.expression(depth + 1)?;
                self.expect(")")?;
                expr
            }
            "certificate" => {
                let index = match self.take()?.as_str() {
                    "leaf" => 0,
                    value => value.parse::<usize>().map_err(|_| UNSUPPORTED)?,
                };
                self.expect("[")?;
                let field = self.take()?;
                self.expect("]")?;
                if let Some(oid) = field.strip_prefix("field.") {
                    oid.parse::<der::asn1::ObjectIdentifier>()
                        .map_err(|_| INVALID)?;
                    self.accept("exists");
                    Expr::CertExtension(index, oid.to_owned())
                } else if field == "subject.OU" && index == 0 {
                    self.expect("=")?;
                    Expr::LeafOu(self.take()?)
                } else {
                    return Err(UNSUPPORTED);
                }
            }
            _ => return Err(UNSUPPORTED),
        })
    }
}

pub(super) fn parse(text: &str) -> Result<Expr, i32> {
    if text.len() > 16384 {
        return Err(INVALID);
    }
    let bytes = text.as_bytes();
    let mut cursor = 0;
    let mut tokens = Vec::new();
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        if bytes[cursor..].starts_with(b"/*") {
            let end = text[cursor + 2..].find("*/").ok_or(INVALID)?;
            cursor += end + 4;
            continue;
        }
        if bytes[cursor] == b'"' {
            cursor += 1;
            let mut token = Vec::new();
            let mut closed = false;
            while cursor < bytes.len() {
                let byte = bytes[cursor];
                cursor += 1;
                if byte == b'"' {
                    closed = true;
                    break;
                }
                if byte == b'\\' {
                    let next = *bytes.get(cursor).ok_or(INVALID)?;
                    if next != b'"' && next != b'\\' {
                        return Err(INVALID);
                    }
                    token.push(next);
                    cursor += 1;
                } else {
                    token.push(byte);
                }
            }
            if !closed {
                return Err(INVALID);
            }
            tokens.push(String::from_utf8(token).map_err(|_| INVALID)?);
        } else if b"()[]=!".contains(&bytes[cursor]) {
            tokens.push((bytes[cursor] as char).to_string());
            cursor += 1;
        } else {
            let start = cursor;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && !b"()[]=!\"".contains(&bytes[cursor])
            {
                cursor += 1;
            }
            tokens.push(text[start..cursor].to_owned());
        }
        if tokens.len() > 128 {
            return Err(INVALID);
        }
    }
    let mut parser = Parser { tokens, cursor: 0 };
    let result = parser.expression(0)?;
    if parser.cursor != parser.tokens.len() {
        return Err(INVALID);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compiled_requirements_enforce_identity_and_reject_truncation() {
        let mut bytes = Vec::new();
        // And(identifier("test"), anchor apple generic).
        for word in [0xfade0c00u32, 32, 1, 6, 2, 4, 0x74657374, 15] {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        let expr = binary(&bytes).unwrap();
        assert!(expr.matches("test", true, &[]).unwrap());
        assert!(!expr.matches("other", true, &[]).unwrap());
        assert!(!expr.matches("test", false, &[]).unwrap());
        for end in 0..bytes.len() {
            assert!(binary(&bytes[..end]).is_err());
        }
        bytes[28..].copy_from_slice(&999u32.to_be_bytes());
        assert!(binary(&bytes).is_err());
    }

    #[test]
    fn requirements_use_boolean_precedence_and_real_identity() {
        let expr =
            parse(r#"identifier wrong or identifier "test" and anchor apple generic"#).unwrap();
        assert!(expr.matches("test", true, &[]).unwrap());
        assert!(!expr.matches("test", false, &[]).unwrap());
        assert!(!expr.matches("other", true, &[]).unwrap());
        assert!(parse("true or unsupported").is_err());
        assert!(parse("identifier \"unterminated").is_err());
    }
}

pub(super) fn binary(data: &[u8]) -> Result<Expr, i32> {
    struct Reader<'a> {
        bytes: &'a [u8],
        offset: usize,
        nodes: usize,
    }
    impl Reader<'_> {
        fn word(&mut self) -> Result<u32, i32> {
            let end = self.offset.checked_add(4).ok_or(INVALID)?;
            let value = u32::from_be_bytes(
                self.bytes
                    .get(self.offset..end)
                    .ok_or(INVALID)?
                    .try_into()
                    .unwrap(),
            );
            self.offset = end;
            Ok(value)
        }
        fn data(&mut self) -> Result<&[u8], i32> {
            let length = self.word()? as usize;
            let end = self.offset.checked_add(length).ok_or(INVALID)?;
            let value = self.bytes.get(self.offset..end).ok_or(INVALID)?;
            self.offset = end.checked_add(3).ok_or(INVALID)? & !3;
            if self.offset > self.bytes.len() {
                return Err(INVALID);
            }
            Ok(value)
        }
        fn string(&mut self) -> Result<String, i32> {
            Ok(std::str::from_utf8(self.data()?)
                .map_err(|_| INVALID)?
                .to_owned())
        }
        fn expression(&mut self, depth: usize) -> Result<Expr, i32> {
            self.nodes += 1;
            if depth > 32 || self.nodes > 128 {
                return Err(INVALID);
            }
            Ok(match self.word()? {
                0 => Expr::Bool(false),
                1 => Expr::Bool(true),
                2 => Expr::Identifier(self.string()?),
                6 => Expr::And(
                    Box::new(self.expression(depth + 1)?),
                    Box::new(self.expression(depth + 1)?),
                ),
                7 => Expr::Or(
                    Box::new(self.expression(depth + 1)?),
                    Box::new(self.expression(depth + 1)?),
                ),
                9 => Expr::Not(Box::new(self.expression(depth + 1)?)),
                15 => Expr::AppleGeneric,
                11 => {
                    let index = self.word()?;
                    let field = self.string()?;
                    let operation = self.word()?;
                    if index != 0 || field != "subject.OU" || operation != 1 {
                        return Err(UNSUPPORTED);
                    }
                    Expr::LeafOu(self.string()?)
                }
                14 => {
                    let index = self.word()? as usize;
                    let oid = der::asn1::ObjectIdentifier::from_bytes(self.data()?)
                        .map_err(|_| INVALID)?
                        .to_string();
                    if self.word()? != 0 {
                        return Err(UNSUPPORTED);
                    }
                    Expr::CertExtension(index, oid)
                }
                _ => return Err(UNSUPPORTED),
            })
        }
    }
    if data.len() > 16384 {
        return Err(INVALID);
    }
    let mut reader = Reader {
        bytes: data,
        offset: 0,
        nodes: 0,
    };
    if reader.word()? != 0xfade0c00 || reader.word()? as usize != data.len() || reader.word()? != 1
    {
        return Err(INVALID);
    }
    let expr = reader.expression(0)?;
    if reader.offset != data.len() {
        return Err(INVALID);
    }
    Ok(expr)
}
