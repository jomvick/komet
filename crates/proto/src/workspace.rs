//! Workspace lifecycle types shared by the engine and its clients.

use serde::{Deserialize, Serialize};

/// The fixed data boundary selected when an engine runtime is assembled.
///
/// Authentication can change while a runtime is alive, but its workspace scope
/// cannot. Switching scopes requires assembling a new runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceScope {
    Local,
    Synced,
    Development,
}

/// Stable information about the engine runtime reached by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    pub device_id: String,
    pub workspace_scope: WorkspaceScope,
}

/// Mode d'accès utilisateur partagé entre le démon et l'interface cliente.
///
/// - `FullAccess` : Tous les contrôleurs sont autorisés.
/// - `ReadOnly` : Lecture seule, pas de modifications.
/// - `Sandboxed` : Environnement isolé avec restrictions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AccessMode {
    FullAccess,
    ReadOnly,
    Sandboxed,
}

/// État global du workspace partagé entre le démon et l'interface cliente.
///
/// Ce struct est synchronisé via le canal IPC (Unix socket / WebSocket)
/// et reflété dans l'interface GPUI via le contexte global.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceState {
    pub project_name: String,
    pub environment: String,
    pub git_branch: String,
    pub access_mode: AccessMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_scope_uses_wire_safe_names() {
        for (scope, encoded) in [
            (WorkspaceScope::Local, "\"local\""),
            (WorkspaceScope::Synced, "\"synced\""),
            (WorkspaceScope::Development, "\"development\""),
        ] {
            assert_eq!(serde_json::to_string(&scope).unwrap(), encoded);
            assert_eq!(
                serde_json::from_str::<WorkspaceScope>(encoded).unwrap(),
                scope
            );
        }
    }

    #[test]
    fn engine_info_uses_camel_case_fields() {
        let info = EngineInfo {
            device_id: "device-1".into(),
            workspace_scope: WorkspaceScope::Local,
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            serde_json::json!({
                "deviceId": "device-1",
                "workspaceScope": "local",
            })
        );
    }
}
