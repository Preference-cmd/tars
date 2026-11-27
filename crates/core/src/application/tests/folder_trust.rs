use super::support::*;
use crate::application::TrustedWorkspaceFolder;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_host_folder_trust_methods_without_store() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();

    let host = SessionHost::new(None);
    assert!(!host.is_workspace_folder_trusted(&ws).await.unwrap());
    assert!(
        host.list_trusted_workspace_folders()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        host.trust_workspace_folder(&ws).await,
        Err(ApplicationError::StoreUnavailable)
    ));
    assert!(matches!(
        host.revoke_workspace_folder_trust(&ws).await,
        Err(ApplicationError::StoreUnavailable)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_host_folder_trust_roundtrip_listing_and_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let ws = dir.path().join("workspace");
    let file = dir.path().join("file.txt");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(&file, "not a directory").unwrap();

    let host = SessionHost::new(Some(store_dir.clone()));

    // Read-only and revoke before DB creation must not create tars.db.
    assert!(!host.is_workspace_folder_trusted(&ws).await.unwrap());
    assert!(
        host.list_trusted_workspace_folders()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!host.revoke_workspace_folder_trust(&ws).await.unwrap());
    assert!(!store_dir.join("tars.db").exists());

    // Non-directory trust request maps to ApplicationError::Store.
    assert!(matches!(
        host.trust_workspace_folder(&file).await,
        Err(ApplicationError::Store(_))
    ));

    // Trusting a directory persists canonicalized record and keeps session list empty.
    host.trust_workspace_folder(&ws).await.unwrap();
    assert!(host.is_workspace_folder_trusted(&ws).await.unwrap());
    assert!(host.list().await.unwrap().is_empty());

    let listed: Vec<TrustedWorkspaceFolder> = host.list_trusted_workspace_folders().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].workspace_root,
        std::fs::canonicalize(&ws).unwrap()
    );
    assert!(listed[0].trusted_at_ms > 0);

    // Revoking removes the record and second revoke returns false.
    assert!(host.revoke_workspace_folder_trust(&ws).await.unwrap());
    assert!(!host.is_workspace_folder_trusted(&ws).await.unwrap());
    assert!(
        host.list_trusted_workspace_folders()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!host.revoke_workspace_folder_trust(&ws).await.unwrap());
}
