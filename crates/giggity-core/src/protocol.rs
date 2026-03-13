use serde::{Deserialize, Serialize};

use crate::model::Snapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderFormat {
    Plain,
    Tmux,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Restart,
    Stop,
    Logs,
    OpenUrl,
    CopyPort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    Ping,
    Query {
        view: Option<String>,
    },
    Render {
        view: Option<String>,
        format: RenderFormat,
    },
    Action {
        action: ActionKind,
        resource_id: String,
        confirm: bool,
    },
    Logs {
        resource_id: String,
        lines: usize,
    },
    ValidateConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerResponse {
    Pong { api_version: u8 },
    Query { snapshot: Snapshot },
    Rendered { output: String },
    ActionResult { message: String },
    Logs { content: String },
    Validation { warnings: Vec<String> },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use crate::protocol::{ClientRequest, RenderFormat, ServerResponse};

    #[test]
    fn request_schema_is_stable() {
        let request = ClientRequest::Render {
            view: Some("default".into()),
            format: RenderFormat::Tmux,
        };
        let json = serde_json::to_string(&request).expect("serialize");

        assert_eq!(
            json,
            r#"{"type":"render","view":"default","format":"tmux"}"#
        );
    }

    #[test]
    fn response_schema_is_stable() {
        let json =
            serde_json::to_string(&ServerResponse::Pong { api_version: 1 }).expect("serialize");
        assert_eq!(json, r#"{"type":"pong","api_version":1}"#);
    }
}
