//! What a provider says it takes, read from `LLM_CHAT`'s optional
//! `accepts` action by [`provider_accepts`](crate::provider_accepts).
//!
//! The answer is one [`Support`] per kind of input: images, a temperature,
//! and a response schema. Unknown is an answer, distinct from no.

use gwead::kernel::KernelError;
use gwead::serde_json::Value;

use crate::agent::SessionError;
use crate::content::ImageFormat;

/// What a provider takes, one field per kind of input.
///
/// Read from the provider's `accepts` answer under the `$config` it ran
/// with; a field the answer does not carry is [`Support::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Accepts {
    /// Whether a message may carry image blocks, in a user message or in a
    /// tool result, and which.
    pub images: Support<ImageInput>,
    /// Whether the provider honours a `chat` input's `temperature`.
    pub temperature: Support<TemperatureInput>,
    /// Whether the provider honours a `chat` input's `response_schema`, and
    /// how.
    pub response_schema: Support<ResponseSchemaInput>,
}

/// A provider's answer about one kind of input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Support<T> {
    /// The provider does not say: it has no `accepts` action, or its
    /// answer has no key for this kind of input.
    Unknown,
    /// The provider takes none.
    No,
    /// The provider takes the input described by the value.
    Yes(T),
}

/// The images a provider takes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageInput {
    /// The formats it takes (the contract's `media_types`), in the order
    /// the provider listed them.
    pub formats: Vec<ImageFormat>,
    /// The source types it takes (the contract's `sources`), in the order
    /// the provider listed them.
    pub sources: Vec<ImageSource>,
}

/// A provider that honours a temperature. No fields yet: a later contract
/// version may add some.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TemperatureInput {}

/// How a provider honours a response schema.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResponseSchemaInput {
    /// Whether it honours a schema beside a non-empty `tools`.
    pub with_tools: bool,
    /// The JSON Schema keywords it honours beyond the contract's floor, in
    /// the order listed; empty for the floor alone.
    pub keywords: Vec<String>,
}

/// How an image block carries its image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageSource {
    /// The bytes inline, as an [`Image`](crate::Image).
    Base64,
    /// A reference the provider resolves, as an
    /// [`ImageReference`](crate::ImageReference).
    Reference,
}

impl ImageSource {
    /// Every source type, in the order the contract's `sources` enum
    /// lists them.
    pub(crate) const ALL: [ImageSource; 2] = [Self::Base64, Self::Reference];

    /// The source type as the contract spells it (`source.type` of an
    /// image block).
    pub(crate) fn source_type(self) -> &'static str {
        match self {
            Self::Base64 => "base64",
            Self::Reference => "reference",
        }
    }
}

/// Why [`provider_accepts`](crate::provider_accepts) gave no answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AcceptsError {
    /// The config is refused before the provider is asked: one of
    /// [`SessionError::NoProvider`], [`SessionError::AmbiguousProvider`],
    /// [`SessionError::NoSuchProvider`] or
    /// [`SessionError::ReservedContextKey`].
    #[error(transparent)]
    Session(SessionError),
    /// The caller's token had fired when the provider's step failed.
    #[error("cancelled")]
    Cancelled,
    /// The provider's `accepts` step failed and the token had not fired.
    #[error("provider step failed: {0}")]
    Step(KernelError),
    /// The provider's answer is not one the contract admits.
    #[error("provider output violates the LLM_CHAT contract: {0}")]
    Contract(String),
}

/// Read an `accepts` answer: an open object, one key per kind of input.
/// An absent key is unknown; a key this version knows is read
/// fail-closed; a key it does not know is ignored.
pub(crate) fn parse(out: &Value) -> Result<Accepts, String> {
    let Some(fields) = out.as_object() else {
        return Err(format!("accepts output is not an object: {out}"));
    };
    let images = match fields.get("images") {
        None => Support::Unknown,
        Some(Value::Bool(false)) => Support::No,
        Some(Value::Object(images)) => {
            let formats = names(
                images.get("media_types"),
                "media_types",
                "media type",
                |v| {
                    let s = v.as_str()?;
                    ImageFormat::ALL
                        .iter()
                        .copied()
                        .find(|f| f.media_type() == s)
                },
            )?;
            let sources = names(images.get("sources"), "sources", "source type", |v| {
                let s = v.as_str()?;
                ImageSource::ALL
                    .iter()
                    .copied()
                    .find(|k| k.source_type() == s)
            })?;
            Support::Yes(ImageInput { formats, sources })
        }
        Some(other) => {
            return Err(format!("`images` is neither false nor an object: {other}"));
        }
    };
    let temperature = match fields.get("temperature") {
        None => Support::Unknown,
        Some(Value::Bool(false)) => Support::No,
        Some(Value::Object(_)) => Support::Yes(TemperatureInput {}),
        Some(other) => {
            return Err(format!(
                "`temperature` is neither false nor an object: {other}"
            ));
        }
    };
    let response_schema = match fields.get("response_schema") {
        None => Support::Unknown,
        Some(Value::Bool(false)) => Support::No,
        Some(Value::Object(schema)) => Support::Yes(response_schema_input(schema)?),
        Some(other) => {
            return Err(format!(
                "`response_schema` is neither false nor an object: {other}"
            ));
        }
    };
    Ok(Accepts {
        images,
        temperature,
        response_schema,
    })
}

/// Read the object a provider answers under `response_schema`.
fn response_schema_input(
    schema: &gwead::serde_json::Map<String, Value>,
) -> Result<ResponseSchemaInput, String> {
    let Some(with_tools) = schema.get("with_tools").and_then(Value::as_bool) else {
        return Err("`response_schema` has no boolean `with_tools`".into());
    };
    let Some(items) = schema.get("keywords").and_then(Value::as_array) else {
        return Err("`response_schema` has no array `keywords`".into());
    };
    let mut keywords: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        match item.as_str() {
            Some(k) if !k.is_empty() => {
                if keywords.iter().any(|seen| seen == k) {
                    return Err(format!("`response_schema` names a keyword twice: {item}"));
                }
                keywords.push(k.to_owned());
            }
            _ => {
                return Err(format!(
                    "`response_schema` names a keyword that is not a non-empty string: {item}"
                ));
            }
        }
    }
    Ok(ResponseSchemaInput {
        with_tools,
        keywords,
    })
}

/// A non-empty array of unique values, each one `known` maps to a `T`.
fn names<T: Copy + PartialEq>(
    value: Option<&Value>,
    field: &str,
    what: &str,
    known: impl Fn(&Value) -> Option<T>,
) -> Result<Vec<T>, String> {
    let items = match value.and_then(Value::as_array) {
        Some(items) if !items.is_empty() => items,
        _ => return Err(format!("`images` has no non-empty array `{field}`")),
    };
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        let Some(t) = known(item) else {
            return Err(format!(
                "`images` names a {what} the contract does not: {item}"
            ));
        };
        if out.contains(&t) {
            return Err(format!("`images` names a {what} twice: {item}"));
        }
        out.push(t);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwead::serde_json::json;

    fn images(v: Value) -> Support<ImageInput> {
        parse(&v).expect("conforming answer").images
    }

    fn temperature(v: Value) -> Support<TemperatureInput> {
        parse(&v).expect("conforming answer").temperature
    }

    fn response_schema(v: Value) -> Support<ResponseSchemaInput> {
        parse(&v).expect("conforming answer").response_schema
    }

    #[test]
    fn an_answer_reads_as_unknown_no_or_yes() {
        for unknown in [
            json!({}),
            json!({"error": {"message": "x"}}),
            json!({"top_p": true}),
            json!({"images": false}),
        ] {
            assert_eq!(temperature(unknown.clone()), Support::Unknown);
            assert_eq!(response_schema(unknown), Support::Unknown);
        }
        assert_eq!(images(json!({})), Support::Unknown);
        assert_eq!(images(json!({"error": {"message": "x"}})), Support::Unknown);
        assert_eq!(images(json!({"top_p": true})), Support::Unknown);
        assert_eq!(temperature(json!({"temperature": false})), Support::No);
        assert_eq!(
            temperature(json!({"temperature": {}})),
            Support::Yes(TemperatureInput {})
        );
        assert_eq!(
            temperature(json!({"temperature": {"max": 2}})),
            Support::Yes(TemperatureInput {})
        );
        assert_eq!(
            response_schema(json!({"response_schema": false})),
            Support::No
        );
        assert_eq!(
            response_schema(json!({"response_schema": {
                "with_tools": false,
                "keywords": ["anyOf", "$ref"],
                "x": 1,
            }})),
            Support::Yes(ResponseSchemaInput {
                with_tools: false,
                keywords: vec!["anyOf".into(), "$ref".into()],
            })
        );
        assert_eq!(
            response_schema(json!({"response_schema": {
                "with_tools": true,
                "keywords": [],
            }})),
            Support::Yes(ResponseSchemaInput {
                with_tools: true,
                keywords: vec![],
            })
        );
        assert_eq!(images(json!({"images": false})), Support::No);
        assert_eq!(
            images(json!({"images": {
                "media_types": ["image/webp", "image/png"],
                "sources": ["reference"],
            }})),
            Support::Yes(ImageInput {
                formats: vec![ImageFormat::Webp, ImageFormat::Png],
                sources: vec![ImageSource::Reference],
            })
        );
        let all: Vec<&str> = ImageFormat::ALL.iter().map(|f| f.media_type()).collect();
        assert_eq!(
            images(json!({"images": {
                "media_types": all,
                "sources": ["base64", "reference"],
                "max_bytes": 5,
            }})),
            Support::Yes(ImageInput {
                formats: ImageFormat::ALL.to_vec(),
                sources: vec![ImageSource::Base64, ImageSource::Reference],
            })
        );
    }

    #[test]
    fn an_off_contract_answer_is_refused() {
        let ok_sources = json!(["base64"]);
        let ok_types = json!(["image/png"]);
        let with = |types: Option<Value>, sources: Option<Value>| {
            let mut m = gwead::serde_json::Map::new();
            if let Some(t) = types {
                m.insert("media_types".into(), t);
            }
            if let Some(s) = sources {
                m.insert("sources".into(), s);
            }
            json!({ "images": m })
        };
        let t = |v: Value| with(Some(v), Some(ok_sources.clone()));
        let s = |v: Value| with(Some(ok_types.clone()), Some(v));
        let no_types = "`images` has no non-empty array `media_types`";
        let no_sources = "`images` has no non-empty array `sources`";
        let cases: Vec<(Value, String)> = vec![
            (json!([]), "accepts output is not an object: []".into()),
            (
                json!({"images": true}),
                "`images` is neither false nor an object: true".into(),
            ),
            (
                json!({"images": null}),
                "`images` is neither false nor an object: null".into(),
            ),
            (
                json!({"images": "yes"}),
                "`images` is neither false nor an object: \"yes\"".into(),
            ),
            (with(None, Some(ok_sources.clone())), no_types.into()),
            (t(json!("image/png")), no_types.into()),
            (t(json!([])), no_types.into()),
            (
                t(json!(["image/bmp"])),
                "`images` names a media type the contract does not: \"image/bmp\"".into(),
            ),
            (
                t(json!([1])),
                "`images` names a media type the contract does not: 1".into(),
            ),
            (
                t(json!(["image/png", "image/png"])),
                "`images` names a media type twice: \"image/png\"".into(),
            ),
            (with(Some(ok_types.clone()), None), no_sources.into()),
            (s(json!("base64")), no_sources.into()),
            (s(json!([])), no_sources.into()),
            (
                s(json!(["url"])),
                "`images` names a source type the contract does not: \"url\"".into(),
            ),
            (
                s(json!(["base64", "base64"])),
                "`images` names a source type twice: \"base64\"".into(),
            ),
        ];
        let rs = |v: Value| json!({ "response_schema": v });
        let no_with_tools = "`response_schema` has no boolean `with_tools`";
        let no_keywords = "`response_schema` has no array `keywords`";
        let bad_keyword = "`response_schema` names a keyword that is not a non-empty string";
        let mut cases = cases;
        for (v, what) in [
            (json!(true), "true"),
            (json!(null), "null"),
            (json!("yes"), "\"yes\""),
            (json!(0), "0"),
        ] {
            cases.push((
                json!({ "temperature": v }),
                format!("`temperature` is neither false nor an object: {what}"),
            ));
        }
        for (v, what) in [(json!(true), "true"), (json!(null), "null")] {
            cases.push((
                rs(v),
                format!("`response_schema` is neither false nor an object: {what}"),
            ));
        }
        cases.extend([
            (rs(json!({"keywords": []})), no_with_tools.into()),
            (
                rs(json!({"with_tools": "yes", "keywords": []})),
                no_with_tools.into(),
            ),
            (rs(json!({"with_tools": true})), no_keywords.into()),
            (
                rs(json!({"with_tools": true, "keywords": "anyOf"})),
                no_keywords.into(),
            ),
            (
                rs(json!({"with_tools": true, "keywords": [""]})),
                format!("{bad_keyword}: \"\""),
            ),
            (
                rs(json!({"with_tools": true, "keywords": [1]})),
                format!("{bad_keyword}: 1"),
            ),
            (
                rs(json!({"with_tools": true, "keywords": ["a", "a"]})),
                "`response_schema` names a keyword twice: \"a\"".into(),
            ),
            (
                json!({
                    "images": {"media_types": ["image/png"], "sources": ["base64"]},
                    "response_schema": true,
                }),
                "`response_schema` is neither false nor an object: true".into(),
            ),
        ]);
        for (answer, text) in cases {
            assert_eq!(parse(&answer), Err(text), "answer: {answer}");
        }
    }
}
