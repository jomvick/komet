//! Integrated regressions for local-first profile privacy and lifecycle boundaries.

use std::path::Path;
use std::sync::{Arc, Barrier};

use komet_engine::{
    EngineConfig, EngineCore, EngineProfile, HarnessId, WorkspaceScope, default_registry,
};

fn config(data_dir: &Path, edge_url: String) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        edge_url,
        edge_token: None,
        ipc_port: 0,
        default_harness: HarnessId::Mock,
        org_id: None,
        sync_token: None,
    }
}

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

async fn shutdown(core: EngineCore) {
    core.shutdown().await;
    drop(core);
}

fn concurrent_engine_info(
    config: Arc<EngineConfig>,
    workers: usize,
) -> std::collections::HashSet<String> {
    let barrier = Arc::new(Barrier::new(workers));
    (0..workers)
        .map(|_| {
            let config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                komet_engine::Engine::engine_info(&config, WorkspaceScope::Local)
                    .expect("resolve concurrent engine info")
                    .device_id
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|call| call.join().expect("engine-info worker"))
        .collect()
}

#[tokio::test]
async fn concurrent_engine_info_and_runtime_share_one_device_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(config(dir.path(), "http://127.0.0.1:1".into()));
    let announced = concurrent_engine_info(config, 32);

    assert_eq!(announced.len(), 1, "every viewport announces one identity");
    let announced = announced.into_iter().next().expect("announced id");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("device-id"))
            .expect("persisted device id")
            .trim(),
        announced
    );

    let core = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    assert_eq!(
        core.device_id, announced,
        "the assembled runtime must use the identity already announced"
    );
    shutdown(core).await;
}

#[tokio::test]
async fn empty_legacy_device_identity_is_repaired_once_for_all_boots() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("device-id"), b"").expect("seed truncated identity");
    let config = Arc::new(config(dir.path(), "http://127.0.0.1:1".into()));

    let announced = concurrent_engine_info(config, 32);
    assert_eq!(
        announced.len(),
        1,
        "legacy repair must publish one identity"
    );
    let announced = announced.into_iter().next().expect("repaired id");
    assert!(!announced.trim().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("device-id"))
            .expect("repaired device id")
            .trim(),
        announced
    );

    let core = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    assert_eq!(core.device_id, announced);
    shutdown(core).await;
}

#[tokio::test]
async fn local_and_synced_profiles_remain_isolated_across_restarts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let local_profile = EngineProfile::local(dir.path()).expect("local profile");
    let local_user_id = local_profile.user_id().to_string();
    let local_profile_file = dir.path().join("local-profile.json");
    let local_profile_bytes = std::fs::read(&local_profile_file).expect("local profile file");

    let local_upload = {
        let core = assemble(local_profile.clone());
        let device_id = core.device_id.clone();
        core.workspace
            .create_space(
                "local-space",
                &device_id,
                "/private/local-project",
                Some("Private project".into()),
                false,
            )
            .expect("create local space");
        core.workspace
            .create_chat("local-chat", Some("local-space"), None, None, None)
            .expect("create local chat");
        core.workspace
            .rename_chat("local-chat", "Private local session")
            .expect("name local chat");
        core.doc_host
            .open("local-chat")
            .expect("open local chat doc")
            .write_user_message("local-message", "Private local transcript", 1)
            .expect("write local transcript");
        core.uploads
            .append("local-upload", "cHJpdmF0ZQ==", Some(0))
            .expect("stage local upload");
        let upload = core
            .uploads
            .commit("local-upload", "private.png")
            .expect("commit local upload");
        assert!(Path::new(&upload).starts_with(local_profile.uploads_root()));
        shutdown(core).await;
        (device_id, upload)
    };

    let synced_profile = EngineProfile::synced(dir.path(), "cloud-org", "cloud-user");
    {
        let core = assemble(synced_profile.clone());
        assert_eq!(core.device_id, local_upload.0, "device identity is global");
        assert!(
            core.workspace
                .chat("local-chat")
                .expect("read synced chats")
                .is_none(),
            "the synced profile must not expose local chats"
        );
        assert_eq!(core.uploads.dir(), synced_profile.uploads_root());
        let error = core
            .uploads
            .read_chunk(&local_upload.1, 0, &[])
            .expect_err("synced upload jail must reject a local-profile path");
        assert!(
            error.to_string().contains("outside the upload cache"),
            "unexpected jail error: {error}"
        );

        core.workspace
            .create_space(
                "synced-space",
                &core.device_id,
                "/shared/cloud-project",
                None,
                false,
            )
            .expect("create synced space");
        core.workspace
            .create_chat("synced-chat", Some("synced-space"), None, None, None)
            .expect("create synced chat");
        shutdown(core).await;
    }

    let reopened_profile = EngineProfile::local(dir.path()).expect("reopen local profile");
    assert_eq!(reopened_profile.user_id(), local_user_id);
    assert_eq!(
        std::fs::read(&local_profile_file).expect("re-read local profile"),
        local_profile_bytes,
        "reopening local must not rotate or rewrite its identity"
    );
    {
        let core = assemble(reopened_profile);
        assert_eq!(core.device_id, local_upload.0);
        let chat = core
            .workspace
            .chat("local-chat")
            .expect("read local chat")
            .expect("local chat survived restart");
        assert_eq!(chat.title.as_deref(), Some("Private local session"));
        assert_eq!(chat.cwd.as_deref(), Some("/private/local-project"));
        let transcript = core
            .doc_host
            .open("local-chat")
            .expect("reopen local chat doc")
            .doc()
            .read_entries()
            .expect("read local transcript");
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].id, "local-message");
        assert!(
            core.workspace
                .chat("synced-chat")
                .expect("read local chats")
                .is_none(),
            "the local profile must not expose synced chats"
        );
        assert_eq!(
            core.uploads
                .read_chunk(&local_upload.1, 0, &[])
                .expect("local profile can read its upload")
                .data,
            "cHJpdmF0ZQ=="
        );
        shutdown(core).await;
    }
}

#[tokio::test]
async fn synced_accounts_isolate_uploads_and_assign_the_legacy_cache_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let legacy_root = dir.path().join("uploads");
    std::fs::create_dir(&legacy_root).expect("legacy uploads root");
    let legacy_upload = legacy_root.join("legacy.png");
    std::fs::write(&legacy_upload, b"legacy").expect("legacy attachment");

    let first_profile = EngineProfile::synced(dir.path(), "org-a", "user-a");
    let first_upload = {
        let core = assemble(first_profile.clone());
        assert_eq!(core.uploads.dir(), first_profile.uploads_root());
        assert_eq!(
            core.uploads
                .read_chunk(legacy_upload.to_str().unwrap(), 0, &[])
                .expect("legacy owner can read the compatibility cache")
                .data,
            "bGVnYWN5"
        );
        core.uploads
            .append("shared-upload", "YWNjb3VudC1h", Some(0))
            .expect("stage first account upload");
        let upload = core
            .uploads
            .commit("shared-upload", "image.png")
            .expect("commit first account upload");
        assert!(Path::new(&upload).starts_with(first_profile.uploads_root()));
        shutdown(core).await;
        upload
    };

    let second_profile = EngineProfile::synced(dir.path(), "org-b", "user-b");
    let second_upload = {
        let core = assemble(second_profile.clone());
        assert_eq!(core.uploads.dir(), second_profile.uploads_root());
        assert_ne!(core.uploads.dir(), first_profile.uploads_root());
        for path in [legacy_upload.to_str().unwrap(), &first_upload] {
            let error = core
                .uploads
                .read_chunk(path, 0, &[])
                .expect_err("another account must not read the attachment");
            assert!(error.to_string().contains("outside the upload cache"));
        }
        core.uploads
            .append("shared-upload", "YWNjb3VudC1i", Some(0))
            .expect("stage second account upload with the same id");
        let upload = core
            .uploads
            .commit("shared-upload", "image.png")
            .expect("commit second account upload with the same name");
        assert!(Path::new(&upload).starts_with(second_profile.uploads_root()));
        assert_ne!(upload, first_upload);
        shutdown(core).await;
        upload
    };

    let core = assemble(first_profile);
    assert_eq!(
        core.uploads
            .read_chunk(&first_upload, 0, &[])
            .expect("first account can reopen its scoped upload")
            .data,
        "YWNjb3VudC1h"
    );
    assert_eq!(
        core.uploads
            .read_chunk(legacy_upload.to_str().unwrap(), 0, &[])
            .expect("legacy ownership survives account switches")
            .data,
        "bGVnYWN5"
    );
    let error = core
        .uploads
        .read_chunk(&second_upload, 0, &[])
        .expect_err("first account must not read the second account upload");
    assert!(error.to_string().contains("outside the upload cache"));
    shutdown(core).await;
}
