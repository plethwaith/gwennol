//! The tools a session offers the model: harvested once, their argument
//! schemas compiled once, and the result shape they must answer with.

use boon::{Compiler, SchemaIndex, Schemas};
use gwead::kernel::Kernel;
use gwead::kernel::registry::ToolDescriptor;
use gwead::serde_json::{Value, json};

use super::SessionError;
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
    /// Harvest the kernel's `TOOL` plugins through the one implementation
    /// of the harvest rules, and compile each declared argument schema.
    /// A schema that does not compile refuses the session: a tool whose
    /// arguments cannot be checked would have to be dispatched
    /// unchecked, which the contract assigns to the caller not to do.
    pub(crate) fn harvest(kernel: &Kernel) -> Result<Self, SessionError> {
        let descriptors = spi::harvest_tools(kernel)?;
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

/// A `TOOL` `call` result, as the contract shapes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub truncated: bool,
}

/// Read a `call` action's output. Fail-closed like every contract
/// shape: a result missing `content`, mistyping a field, or carrying
/// one the closed schema does not allow is not a result.
pub(crate) fn parse_output(output: &Value) -> Result<ToolOutput, String> {
    let Some(fields) = output.as_object() else {
        return Err(format!("result is not an object: {output}"));
    };
    let mut out = ToolOutput {
        content: String::new(),
        is_error: false,
        truncated: false,
    };
    let mut has_content = false;
    for (key, value) in fields {
        match (key.as_str(), value) {
            ("content", Value::String(s)) => {
                out.content = s.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_are_read_fail_closed() {
        assert_eq!(
            parse_output(&json!({"content": "c"})).unwrap(),
            ToolOutput {
                content: "c".into(),
                is_error: false,
                truncated: false
            }
        );
        assert_eq!(
            parse_output(&json!({"content": "c", "is_error": true, "truncated": true})).unwrap(),
            ToolOutput {
                content: "c".into(),
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
}
