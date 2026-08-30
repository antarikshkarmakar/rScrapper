use crate::{Error, Result};
use encoding_rs::{Encoding, UTF_8};
use mime::Mime;

pub(crate) struct ValidatedContentType {
    pub(crate) declaration: String,
    pub(crate) identity: ContentTypeIdentity,
    pub(crate) encoding: &'static Encoding,
}

#[derive(PartialEq, Eq)]
pub(crate) struct ContentTypeIdentity {
    pub(crate) media_type: MediaTypeIdentity,
    charset: Option<String>,
}

#[derive(PartialEq, Eq)]
pub(crate) struct MediaTypeIdentity {
    type_: String,
    subtype: String,
    suffix: Option<String>,
}

pub(crate) fn validate_content_type_declarations(
    declarations: &[&str],
) -> Result<ValidatedContentType> {
    let first = declarations
        .first()
        .ok_or_else(|| Error::Policy("response content type is required".into()))?;
    let (identity, encoding) = parse_content_type(first)?;
    for declaration in &declarations[1..] {
        if parse_content_type(declaration)?.0 != identity {
            return Err(Error::Policy("conflicting response content types".into()));
        }
    }
    Ok(ValidatedContentType {
        declaration: (*first).to_owned(),
        identity,
        encoding,
    })
}

pub(crate) fn reject_attachments(declarations: &[&str]) -> Result<()> {
    if declarations.iter().any(|declaration| {
        declaration
            .split(';')
            .next()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("attachment"))
    }) {
        return Err(Error::Policy(
            "response attachments are not permitted".into(),
        ));
    }
    Ok(())
}

impl MediaTypeIdentity {
    pub(crate) fn is_supported_http(&self) -> bool {
        let concrete_subtype = !self.subtype.is_empty() && !self.subtype.contains('*');
        if self.type_ == "text" {
            return concrete_subtype;
        }
        if self.type_ != "application" {
            return false;
        }
        match self.suffix.as_deref() {
            None => self.subtype == "json" || self.subtype == "xml",
            Some("json" | "xml") => concrete_subtype,
            Some(_) => false,
        }
    }

    pub(crate) fn is_html_document(&self) -> bool {
        (self.type_ == "text" && self.subtype == "html" && self.suffix.is_none())
            || (self.type_ == "application"
                && self.subtype == "xhtml"
                && self.suffix.as_deref() == Some("xml"))
    }
}

fn parse_content_type(content_type: &str) -> Result<(ContentTypeIdentity, &'static Encoding)> {
    if content_type.trim_end().ends_with(';') {
        return Err(Error::Policy("invalid response content type".into()));
    }
    let parsed: Mime = content_type
        .parse()
        .map_err(|_| Error::Policy("invalid response content type".into()))?;
    let media_type = MediaTypeIdentity {
        type_: parsed.type_().as_str().to_ascii_lowercase(),
        subtype: parsed.subtype().as_str().to_ascii_lowercase(),
        suffix: parsed
            .suffix()
            .map(|suffix| suffix.as_str().to_ascii_lowercase()),
    };
    let mut charset = None;
    let mut encoding = UTF_8;
    for (name, value) in parsed.params() {
        if name != mime::CHARSET {
            continue;
        }
        let (candidate, candidate_encoding) = canonical_charset(value.as_str());
        match &charset {
            Some(current) if current != &candidate => {
                return Err(Error::Policy(
                    "conflicting response charset parameters".into(),
                ));
            }
            Some(_) => {}
            None => {
                charset = Some(candidate);
                encoding = candidate_encoding;
            }
        }
    }
    Ok((
        ContentTypeIdentity {
            media_type,
            charset,
        },
        encoding,
    ))
}

fn canonical_charset(label: &str) -> (String, &'static Encoding) {
    match Encoding::for_label(label.as_bytes()) {
        Some(encoding) => (encoding.name().to_ascii_lowercase(), encoding),
        None => (label.to_ascii_lowercase(), UTF_8),
    }
}
