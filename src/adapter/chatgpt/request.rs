use super::{NAMESPACE, invalid};
use crate::core::{
    Capabilities, ChatContent, GatewayError, Request, RouteIdentity, RoutedRequest, ToolChoice,
    TrustZone,
};
use serde_json::{Value, json};

pub(super) struct RouteBinding {
    identity: RouteIdentity,
    effort: Option<crate::core::ReasoningEffort>,
}

impl RouteBinding {
    pub(super) fn from_route(route: &crate::config::ValidatedRoute) -> Self {
        Self {
            identity: route.identity().clone(),
            effort: route.reasoning_effort(),
        }
    }
}

pub(super) fn encode(
    routed: &RoutedRequest,
    capabilities: &Capabilities,
    routes: &[RouteBinding],
) -> Result<Value, GatewayError> {
    let Request::Chat(chat) = routed.request() else {
        return Err(invalid());
    };
    let binding = routes
        .iter()
        .find(|binding| binding.identity == routed.context().route)
        .ok_or_else(invalid)?;
    if chat.options.response_format.is_some()
        || chat.options.sampling_param().is_some()
        || chat
            .options
            .reasoning_effort
            .is_some_and(|effort| Some(effort) != binding.effort)
        || chat.options.enable_thinking.is_some()
        || capabilities.check_request(routed.request()).is_err()
        || routed.context().trust_zone != TrustZone::External
        || routed.context().check_request(routed.request()).is_err()
        || (!capabilities.function_tools
            && chat.messages.iter().any(|message| {
                message.content.iter().any(|part| {
                    matches!(
                        part,
                        ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. }
                    )
                })
            }))
    {
        return Err(invalid());
    }
    let mut payload = crate::adapter::codex::protocol::to_text_request(routed)?;
    if let Some(effort) = binding.effort {
        if !capabilities.reasoning_control {
            return Err(invalid());
        }
        payload["reasoning"] = json!({"effort":effort});
    }
    if let Some(tools) = payload.get_mut("tools") {
        let mut functions = tools.take();
        if let ToolChoice::Function { name } = &chat.tool_choice {
            functions
                .as_array_mut()
                .ok_or_else(invalid)?
                .retain(|tool| tool["name"] == *name);
        }
        for function in functions.as_array_mut().ok_or_else(invalid)? {
            function["strict"] = json!(false);
        }
        *tools = json!([{"type": "namespace", "name": NAMESPACE,
            "description": "Functions supplied by the client.", "tools": functions}]);
    }
    if matches!(chat.tool_choice, ToolChoice::Function { .. }) {
        payload["tool_choice"] = json!("required");
    }
    for item in payload["input"].as_array_mut().ok_or_else(invalid)? {
        if item["type"] == "function_call" {
            item["namespace"] = json!(NAMESPACE);
        }
    }
    Ok(payload)
}
