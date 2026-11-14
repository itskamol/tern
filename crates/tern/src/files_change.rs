//! One change to a host's files (new folder, rename, delete) followed by whatever the caller does
//! next. Both places that show files run changes through here, so a failure reads the same in
//! each; where it is shown and how the folder is listed again stay with the caller.

use std::sync::Arc;

use tern_ssh::{Call, FileSource};

/// Runs `work` on `source`, then hands `then` the failure text, if any: always, so the caller
/// lists the folder again whether the change worked or not.
pub async fn change_then(
    rt: &tokio::runtime::Handle,
    source: Arc<dyn FileSource>,
    what: &'static str,
    work: impl FnOnce(Arc<dyn FileSource>) -> Call<'static, ()> + Send + 'static,
    then: impl FnOnce(Option<String>),
) {
    let failure = match rt.spawn(work(source)).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, what, "files_change_failed");
            Some(format!("Could not {what}: {e}"))
        }
        Err(_) => Some(format!("Could not {what}")),
    };
    then(failure);
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use tern_ssh::LocalFs;

    use super::*;

    async fn run(
        source: &Arc<dyn FileSource>,
        work: impl FnOnce(Arc<dyn FileSource>) -> Call<'static, ()> + Send + 'static,
    ) -> Vec<Option<String>> {
        let seen = RefCell::new(Vec::new());
        let rt = tokio::runtime::Handle::current();
        change_then(&rt, source.clone(), "rename", work, |f| {
            seen.borrow_mut().push(f)
        })
        .await;
        seen.into_inner()
    }

    #[tokio::test]
    async fn the_caller_goes_on_to_list_again_after_a_change_that_worked() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a").to_string_lossy().into_owned();
        let to = dir.path().join("b").to_string_lossy().into_owned();
        std::fs::write(&from, "x").unwrap();
        let source: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        let seen = run(&source, move |s| {
            Box::pin(async move { s.rename(&from, &to).await })
        })
        .await;
        assert_eq!(seen, [None]);
        assert!(dir.path().join("b").exists());
    }

    #[tokio::test]
    async fn the_caller_goes_on_to_list_again_after_a_change_that_failed_and_is_told_why() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope").to_string_lossy().into_owned();
        let to = dir.path().join("b").to_string_lossy().into_owned();
        let source: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        let seen = run(&source, move |s| {
            Box::pin(async move { s.rename(&missing, &to).await })
        })
        .await;
        assert_eq!(seen.len(), 1);
        let text = seen[0].as_deref().unwrap();
        assert!(text.starts_with("Could not rename: "), "{text}");
        assert!(!text.ends_with('.'), "{text}");
    }
}
