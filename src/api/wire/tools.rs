use serde::Deserialize;
use serde_json::Value;

use crate::core::{FunctionTool, ToolChoice};

use super::valid_name;

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum ToolChoiceWire {
    String(String),
    Named(NamedToolChoice),
}

impl Default for ToolChoiceWire {
    fn default() -> Self {
        Self::String("auto".into())
    }
}

impl ToolChoiceWire {
    pub(super) fn into_core(self, tools: &[FunctionTool]) -> Result<ToolChoice, ()> {
        match self {
            Self::String(value) => match value.as_str() {
                "none" => Ok(ToolChoice::None),
                "auto" => Ok(ToolChoice::Auto),
                "required" if !tools.is_empty() => Ok(ToolChoice::Required),
                _ => Err(()),
            },
            Self::Named(choice)
                if choice.kind == "function"
                    && tools.iter().any(|tool| tool.name == choice.function.name) =>
            {
                Ok(ToolChoice::Function {
                    name: choice.function.name,
                })
            }
            Self::Named(_) => Err(()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NamedToolChoice {
    #[serde(rename = "type")]
    kind: String,
    function: NamedFunction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedFunction {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ToolWire {
    #[serde(rename = "type")]
    kind: String,
    function: FunctionWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FunctionWire {
    name: String,
    #[serde(default)]
    description: Option<String>,
    parameters: Value,
}

impl ToolWire {
    pub(super) fn into_core(self) -> Result<FunctionTool, ()> {
        if self.kind != "function"
            || !valid_name(&self.function.name)
            || !self.function.parameters.is_object()
        {
            return Err(());
        }
        Ok(FunctionTool {
            name: self.function.name,
            description: self.function.description,
            parameters: self.function.parameters,
        })
    }
}
