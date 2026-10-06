use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSessionRequest {
    pub request_id: String,
    pub request: thinkterm_import::ImportRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetImportSessionStatus {
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportSessionStatus {
    NotFound,
    Running,
    Completed(ImportSessionResponse),
    Failed(String),
    Interrupted { space_id: Option<String> },
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetImportSessionStatusResponse {
    pub status: ImportSessionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSessionResponse {
    pub tree: crate::ThinkTermTree,
    pub space_id: String,
    pub workspace: String,
    pub live: bool,
    pub pane_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListImportSessions {
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListImportSessionsResponse {
    pub sessions: Vec<thinkterm_import::Session>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewImportSession {
    pub source: String,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewImportSessionResponse {
    pub preview: thinkterm_import::Preview,
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DecodedPdu, Pdu};

    #[test]
    fn generic_source_request_roundtrips() {
        let pdu = Pdu::ImportSessionRequest(ImportSessionRequest {
            request_id: "1-00000000-0000-4000-8000-000000000001".into(),
            request: thinkterm_import::ImportRequest {
                source: "example".into(),
                session: "development".into(),
                mode: thinkterm_import::ImportMode::Live,
                fingerprint: "example-fingerprint".into(),
                space_name: "Imported".into(),
            },
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 7).unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu { serial: 7, pdu }
        );
    }
    #[test]
    fn import_result_queries_roundtrip() {
        let mut pdus = vec![Pdu::GetImportSessionStatus(GetImportSessionStatus {
            request_id: "1-00000000-0000-4000-8000-000000000001".into(),
        })];
        for status in [
            ImportSessionStatus::NotFound,
            ImportSessionStatus::Running,
            ImportSessionStatus::Failed("example failure".into()),
            ImportSessionStatus::Interrupted {
                space_id: Some("space-example".into()),
            },
            ImportSessionStatus::Expired,
            ImportSessionStatus::Completed(ImportSessionResponse {
                tree: crate::ThinkTermTree::default(),
                space_id: "space-example".into(),
                workspace: "workspace-example".into(),
                live: true,
                pane_count: 1,
            }),
        ] {
            pdus.push(Pdu::GetImportSessionStatusResponse(
                GetImportSessionStatusResponse { status },
            ));
        }
        for pdu in pdus {
            let mut bytes = vec![];
            pdu.encode(&mut bytes, 3).unwrap();
            assert_eq!(
                Pdu::decode(bytes.as_slice()).unwrap(),
                DecodedPdu { serial: 3, pdu }
            );
        }
    }

    #[test]
    fn remote_discovery_and_preview_roundtrip() {
        let preview = thinkterm_import::Preview {
            session: "development".into(),
            live: true,
            version: Some("example-version".into()),
            fingerprint: "example-fingerprint".into(),
            unavailable: None,
            projects: vec![thinkterm_import::PreviewProject {
                name: "example".into(),
                threads: 1,
                tabs: 1,
                panes: 1,
                terminals: vec![thinkterm_import::PreviewTerminal {
                    title: Some("shell".into()),
                    tab_name: None,
                    cwd: Some("/home/user/example".into()),
                }],
            }],
        };
        for pdu in vec![
            Pdu::ListImportSessions(ListImportSessions {
                source: "example".into(),
            }),
            Pdu::ListImportSessionsResponse(ListImportSessionsResponse {
                sessions: vec![thinkterm_import::Session {
                    name: "development".into(),
                }],
                notes: vec![],
            }),
            Pdu::PreviewImportSession(PreviewImportSession {
                source: "example".into(),
                session: "development".into(),
            }),
            Pdu::PreviewImportSessionResponse(PreviewImportSessionResponse {
                preview,
                notes: vec!["session-import-requirements-remote".into()],
            }),
        ] {
            let mut bytes = vec![];
            pdu.encode(&mut bytes, 11).unwrap();
            assert_eq!(
                Pdu::decode(bytes.as_slice()).unwrap(),
                DecodedPdu { serial: 11, pdu }
            );
        }
    }
}
