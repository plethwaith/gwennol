//! The tools a session offers the model: harvested once, their argument
//! schemas compiled once, and the result shape they must answer with.

use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use boon::{Compiler, SchemaIndex, Schemas};
use gwead::kernel::Kernel;
use gwead::kernel::registry::ToolDescriptor;
use gwead::serde_json::{Value, json};

use super::SessionError;
use super::transcript::check_image_source;
use crate::content::{Image, ImageFormat, ImageReference, ToolContent, UserContent};
use crate::spi;

/// One tool the model may call.
pub(crate) struct Tool {
    pub descriptor: ToolDescriptor,
    schema: SchemaIndex,
}

/// Every tool of the session, in the harvest's (sorted) order, with the
/// `tools` wire entry the provider gets.
pub(crate) struct ToolTable {
    tools: Vec<Tool>,
    schemas: Schemas,
    wire: Value,
}

impl ToolTable {
    /// Harvest the kernel's `TOOL` plugins — all of them, or the plugins
    /// `selection` names — through the one implementation of the harvest
    /// rules, and compile each declared argument schema.
    /// A schema that does not compile refuses the session: a tool whose
    /// arguments cannot be checked would have to be dispatched
    /// unchecked, which the contract assigns to the caller not to do.
    pub(crate) fn harvest(
        kernel: &Kernel,
        selection: Option<&BTreeSet<String>>,
    ) -> Result<Self, SessionError> {
        let descriptors = match selection {
            Some(plugins) => spi::harvest_selected(kernel, plugins)?,
            None => spi::harvest_tools(kernel)?,
        };
        let mut compiler = Compiler::new();
        let mut schemas = Schemas::new();
        let mut tools = Vec::with_capacity(descriptors.len());
        for (index, descriptor) in descriptors.into_iter().enumerate() {
            // Addressed by position: a tool name is model-facing text,
            // not necessarily a URL-safe one.
            let url = format!("http://gwennol.dev/session/tools/{index}.json");
            let tool_schema = |error: String| SessionError::ToolSchema {
                tool: descriptor.tool_name.clone(),
                error,
            };
            compiler
                .add_resource(&url, descriptor.parameters.clone())
                .map_err(|e| tool_schema(e.to_string()))?;
            let schema = compiler
                .compile(&url, &mut schemas)
                .map_err(|e| tool_schema(e.to_string()))?;
            tools.push(Tool { descriptor, schema });
        }
        let wire = Value::Array(
            tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.descriptor.tool_name,
                        "description": t.descriptor.description,
                        "input_schema": t.descriptor.parameters,
                    })
                })
                .collect(),
        );
        Ok(Self {
            tools,
            schemas,
            wire,
        })
    }

    /// The `tools` input for `chat`.
    pub(crate) fn wire(&self) -> &Value {
        &self.wire
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub(crate) fn find(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.descriptor.tool_name == name)
    }

    /// Check model-emitted arguments against the tool's declared schema.
    pub(crate) fn validate(&self, tool: &Tool, input: &Value) -> Result<(), String> {
        self.schemas
            .validate(input, tool.schema)
            .map_err(|e| e.to_string())
    }
}

/// A tool's result, as the contract shapes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOutput {
    pub content: ToolContent,
    pub is_error: bool,
    pub truncated: bool,
}

/// Read the output of an action run as a tool. Fail-closed like every contract
/// shape: a result that is not an object, has no `content` or an empty
/// `content` list, mistypes a field, or carries one the closed schema does
/// not allow is not a result. A `content` list is
/// read block by block, by `parse_block`.
pub(crate) fn parse_output(output: &Value) -> Result<ToolOutput, String> {
    let Some(fields) = output.as_object() else {
        return Err(format!("result is not an object: {output}"));
    };
    let mut out = ToolOutput {
        content: ToolContent::Text(String::new()),
        is_error: false,
        truncated: false,
    };
    let mut has_content = false;
    for (key, value) in fields {
        match (key.as_str(), value) {
            ("content", Value::String(s)) => {
                out.content = ToolContent::Text(s.clone());
                has_content = true;
            }
            ("content", Value::Array(items)) => {
                if items.is_empty() {
                    return Err("result `content` is an empty list".into());
                }
                out.content = ToolContent::Blocks(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, item)| parse_block(i, item))
                        .collect::<Result<_, _>>()?,
                );
                has_content = true;
            }
            ("is_error", Value::Bool(b)) => out.is_error = *b,
            ("truncated", Value::Bool(b)) => out.truncated = *b,
            ("content" | "is_error" | "truncated", other) => {
                return Err(format!("result field {key:?} has the wrong type: {other}"));
            }
            _ => {
                return Err(format!(
                    "result carries a field the contract does not allow: {key:?}"
                ));
            }
        }
    }
    if !has_content {
        return Err("result has no `content`".into());
    }
    Ok(out)
}

/// Read block `i` of a result's `content` list: a `text` block, or an `image`
/// block whose inline bytes are checked against the format it declares.
fn parse_block(i: usize, item: &Value) -> Result<UserContent, String> {
    let fields = item.as_object();
    let kind = fields.and_then(|f| f.get("type")).and_then(Value::as_str);
    let (Some(fields), Some(kind @ ("text" | "image"))) = (fields, kind) else {
        return Err(format!(
            "result block {i} is not a text or image block: {item}"
        ));
    };
    let allowed: &[&str] = if kind == "text" {
        &["type", "text"]
    } else {
        &["type", "source"]
    };
    if let Some(key) = fields.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!(
            "result block {i} carries a field the contract does not allow: {key:?}"
        ));
    }
    if kind == "text" {
        return match fields.get("text") {
            Some(Value::String(text)) => Ok(UserContent::Text(text.clone())),
            _ => Err(format!("result block {i} has no string `text`")),
        };
    }
    let source = fields.get("source");
    check_image_source(source).map_err(|e| format!("result block {i}: {e}"))?;
    let source = source.and_then(Value::as_object);
    let field = |name: &str| {
        source
            .and_then(|s| s.get(name))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    // `check_image_source` has named the media type and the payload field.
    let declared = ImageFormat::from_media_type(field("media_type"))
        .ok_or_else(|| format!("result block {i}: unreachable media type"))?;
    if field("type") == "reference" {
        return ImageReference::new(declared, field("reference"))
            .map(UserContent::ImageReference)
            .map_err(|e| format!("result block {i}: {e}"));
    }
    let bytes = STANDARD
        .decode(field("data"))
        .map_err(|e| format!("result block {i}: image data is not standard padded base64: {e}"))?;
    let image = Image::new(bytes).map_err(|e| format!("result block {i}: {e}"))?;
    if image.format() != declared {
        return Err(format!(
            "result block {i}: image declares {} but its bytes are {}",
            declared.media_type(),
            image.format().media_type()
        ));
    }
    Ok(UserContent::Image(image))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_are_read_fail_closed() {
        assert_eq!(
            parse_output(&json!({"content": "c"})).unwrap(),
            ToolOutput {
                content: ToolContent::Text("c".into()),
                is_error: false,
                truncated: false
            }
        );
        assert_eq!(
            parse_output(&json!({"content": "c", "is_error": true, "truncated": true})).unwrap(),
            ToolOutput {
                content: ToolContent::Text("c".into()),
                is_error: true,
                truncated: true
            }
        );
        for (bad, why) in [
            (json!({"is_error": true}), "no `content`"),
            (json!({"content": 1}), "wrong type"),
            (json!({"content": "c", "truncated": "yes"}), "wrong type"),
            (json!({"content": "c", "exit": 0}), "does not allow"),
            (json!("c"), "not an object"),
        ] {
            let err = parse_output(&bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }
    const PNG: &str = "iVBORw0KGgo=";

    fn text(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }
    fn inline(media_type: &str, data: &str) -> Value {
        json!({"type": "image", "source": {
            "type": "base64", "media_type": media_type, "data": data}})
    }
    fn reference(media_type: &str, reference: &str) -> Value {
        json!({"type": "image", "source": {
            "type": "reference", "media_type": media_type, "reference": reference}})
    }

    #[test]
    fn a_result_of_blocks_is_read_in_order() {
        let read = parse_output(&json!({"content": [
            text("a"), inline("image/png", PNG), reference("image/jpeg", "k")
        ]}))
        .unwrap();
        let ToolContent::Blocks(items) = read.content else {
            panic!("a list is read as blocks");
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], UserContent::Text("a".into()));
        let UserContent::Image(image) = &items[1] else {
            panic!("{:?}", items[1]);
        };
        assert_eq!(image.format(), ImageFormat::Png);
        assert_eq!(
            image.bytes(),
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        assert_eq!(
            items[2],
            UserContent::ImageReference(ImageReference::new(ImageFormat::Jpeg, "k").unwrap())
        );

        let texts = parse_output(&json!({"content": [text("a"), text("b")]})).unwrap();
        assert_eq!(
            texts.content,
            ToolContent::Blocks(vec![
                UserContent::Text("a".into()),
                UserContent::Text("b".into())
            ])
        );

        let formats = parse_output(&json!({"content": [
            inline("image/jpeg", "/9j/4A=="),
            inline("image/png", PNG),
            inline("image/gif", "R0lGODlh"),
            inline("image/webp", "UklGRgAAAABXRUJQ"),
        ]}))
        .unwrap();
        let ToolContent::Blocks(items) = formats.content else {
            panic!("a list is read as blocks");
        };
        let read: Vec<ImageFormat> = items
            .iter()
            .map(|item| match item {
                UserContent::Image(image) => image.format(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            read,
            [
                ImageFormat::Jpeg,
                ImageFormat::Png,
                ImageFormat::Gif,
                ImageFormat::Webp
            ]
        );

        let flags =
            parse_output(&json!({"content": [text("a")], "is_error": true, "truncated": true}))
                .unwrap();
        assert!(flags.is_error && flags.truncated);
    }

    #[test]
    fn an_off_contract_block_is_refused() {
        let with = |mutate: &dyn Fn(&mut Value)| {
            let mut b = inline("image/png", PNG);
            mutate(&mut b);
            b
        };
        let padding = "result block 0: image data is not standard padded base64: ";
        let cases: Vec<(&str, Value, String)> = vec![
            (
                "an empty list",
                json!([]),
                "result `content` is an empty list".into(),
            ),
            (
                "a string item",
                json!(["a"]),
                "result block 0 is not a text or image block: \"a\"".into(),
            ),
            (
                "no type",
                json!([{"text": "a"}]),
                "result block 0 is not a text or image block: {\"text\":\"a\"}".into(),
            ),
            (
                "an unknown type",
                json!([{"type": "opaque"}]),
                "result block 0 is not a text or image block: {\"type\":\"opaque\"}".into(),
            ),
            (
                "a tool_result",
                json!([{"type": "tool_result"}]),
                "result block 0 is not a text or image block: {\"type\":\"tool_result\"}".into(),
            ),
            (
                "text with no text",
                json!([{"type": "text"}]),
                "result block 0 has no string `text`".into(),
            ),
            (
                "text with a number",
                json!([{"type": "text", "text": 1}]),
                "result block 0 has no string `text`".into(),
            ),
            (
                "text with an extra field",
                json!([{"type": "text", "text": "a", "x": 1}]),
                "result block 0 carries a field the contract does not allow: \"x\"".into(),
            ),
            (
                "an image with no source",
                json!([{"type": "image"}]),
                "result block 0: image block has no object `source`".into(),
            ),
            (
                "an image with an extra field",
                json!([with(&|b| b["x"] = json!(1))]),
                "result block 0 carries a field the contract does not allow: \"x\"".into(),
            ),
            (
                "a source of type url",
                json!([with(&|b| b["source"]["type"] = json!("url"))]),
                "result block 0: image source type is not `base64` or `reference`".into(),
            ),
            (
                "a source with no media type",
                json!([with(&|b| {
                    b["source"].as_object_mut().unwrap().remove("media_type");
                })]),
                "result block 0: image block has a media type the contract does not name: (none)"
                    .into(),
            ),
            (
                "a media type the contract does not name",
                json!([with(&|b| b["source"]["media_type"] = json!("image/bmp"))]),
                "result block 0: image block has a media type the contract does not name: image/bmp"
                    .into(),
            ),
            (
                "base64 with no data",
                json!([with(&|b| {
                    b["source"].as_object_mut().unwrap().remove("data");
                })]),
                "result block 0: image block has no non-empty string `data`".into(),
            ),
            (
                "empty data",
                json!([with(&|b| b["source"]["data"] = json!(""))]),
                "result block 0: image block has no non-empty string `data`".into(),
            ),
            (
                "a reference with no reference",
                json!([{"type": "image", "source": {
                    "type": "reference", "media_type": "image/png"}}]),
                "result block 0: image block has no non-empty string `reference`".into(),
            ),
            (
                "a reference carrying data",
                json!([{"type": "image", "source": {
                    "type": "reference", "media_type": "image/png",
                    "reference": "k", "data": "AA=="}}]),
                "result block 0: image source carries a field the contract does not allow: \"data\""
                    .into(),
            ),
            (
                "data missing its padding",
                json!([inline("image/png", "iVBORw0KGgo")]),
                padding.into(),
            ),
            (
                "data with a space in it",
                json!([inline("image/png", "iVBO Rw0KGgo=")]),
                padding.into(),
            ),
            (
                "data with non-zero trailing bits",
                json!([inline("image/png", "iVBORw0KGgp=")]),
                padding.into(),
            ),
            (
                "bytes no format names",
                json!([inline("image/png", "Qk0=")]),
                "result block 0: the image is not a JPEG, PNG, GIF or WebP file".into(),
            ),
            (
                "PNG bytes declared as JPEG",
                json!([inline("image/jpeg", PNG)]),
                "result block 0: image declares image/jpeg but its bytes are image/png".into(),
            ),
            (
                "the fault behind a valid block",
                json!([text("a"), {"type": "text"}]),
                "result block 1 has no string `text`".into(),
            ),
        ];
        for (name, content, why) in cases {
            let err = parse_output(&json!({"content": content}))
                .err()
                .unwrap_or_else(|| panic!("{name}: the result was accepted"));
            if why == padding {
                assert!(err.starts_with(&why), "{name}: {err}");
            } else {
                assert_eq!(err, why, "{name}");
            }
        }
    }
}
