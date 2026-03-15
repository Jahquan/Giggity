use serde::{Deserialize, Serialize};

use crate::model::{RecentEvent, Snapshot};

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
    ForceKill,
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
    ExportConfig,
    MuteNotifications {
        duration_secs: u64,
    },
    UnmuteNotifications,
    StreamLogs {
        resource_id: String,
        lines: u32,
    },
    BulkRestart {
        resource_ids: Vec<String>,
    },
    StreamEvents {
        view: Option<String>,
    },
    CloseStream,
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
    ExportedConfig { toml: String },
    MuteResult { message: String },
    Error { message: String },
    LogLine { line: String },
    Event { event: RecentEvent },
    ConfigReloaded,
    StreamEnd { reason: String },
}

#[cfg(test)]
mod tests {
    use crate::model::{HealthState, RecentEvent};
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

    #[test]
    fn stream_logs_request_schema_is_stable() {
        let request = ClientRequest::StreamLogs {
            resource_id: "docker:web".into(),
            lines: 50,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"stream_logs","resource_id":"docker:web","lines":50}"#
        );
    }

    #[test]
    fn stream_events_request_schema_is_stable() {
        let request = ClientRequest::StreamEvents {
            view: Some("ops".into()),
        };
        let json = serde_json::to_string(&request).expect("serialize");
        assert_eq!(json, r#"{"type":"stream_events","view":"ops"}"#);

        let none_view = ClientRequest::StreamEvents { view: None };
        let json = serde_json::to_string(&none_view).expect("serialize");
        assert_eq!(json, r#"{"type":"stream_events","view":null}"#);
    }

    #[test]
    fn close_stream_request_schema_is_stable() {
        let json = serde_json::to_string(&ClientRequest::CloseStream).expect("serialize");
        assert_eq!(json, r#"{"type":"close_stream"}"#);
    }

    #[test]
    fn log_line_response_schema_is_stable() {
        let json = serde_json::to_string(&ServerResponse::LogLine {
            line: "hello world".into(),
        })
        .expect("serialize");
        assert_eq!(json, r#"{"type":"log_line","line":"hello world"}"#);
    }

    #[test]
    fn event_response_schema_is_stable() {
        let event = RecentEvent {
            resource_id: "docker:web".into(),
            resource_name: "web".into(),
            from: Some(HealthState::Healthy),
            to: HealthState::Crashed,
            timestamp: chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            cause: Some("state transition".into()),
        };
        let response = ServerResponse::Event { event };
        let json = serde_json::to_string(&response).expect("serialize");
        let roundtrip: ServerResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(response, roundtrip);
    }

    #[test]
    fn config_reloaded_response_schema_is_stable() {
        let json = serde_json::to_string(&ServerResponse::ConfigReloaded).expect("serialize");
        assert_eq!(json, r#"{"type":"config_reloaded"}"#);
    }

    #[test]
    fn stream_end_response_schema_is_stable() {
        let json = serde_json::to_string(&ServerResponse::StreamEnd {
            reason: "client requested close".into(),
        })
        .expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"stream_end","reason":"client requested close"}"#
        );
    }

    #[test]
    fn all_streaming_responses_round_trip_through_json() {
        let responses = vec![
            ServerResponse::LogLine {
                line: "test line".into(),
            },
            ServerResponse::ConfigReloaded,
            ServerResponse::StreamEnd {
                reason: "done".into(),
            },
        ];
        for response in responses {
            let json = serde_json::to_string(&response).expect("serialize");
            let roundtrip: ServerResponse = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(response, roundtrip);
        }
    }

    #[test]
    fn all_streaming_requests_round_trip_through_json() {
        let requests = vec![
            ClientRequest::StreamLogs {
                resource_id: "docker:web".into(),
                lines: 100,
            },
            ClientRequest::StreamEvents {
                view: Some("default".into()),
            },
            ClientRequest::StreamEvents { view: None },
            ClientRequest::CloseStream,
        ];
        for request in requests {
            let json = serde_json::to_string(&request).expect("serialize");
            let roundtrip: ClientRequest = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(request, roundtrip);
        }
    }
}
