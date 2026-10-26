use crate::error::{DbmError, Result};

pub(crate) async fn run_blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            DbmError::Execution(format!(
                "blocking database worker did not complete successfully: {error}"
            ))
        })?
}

#[cfg(test)]
mod tests {
    use super::run_blocking;

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_database_work_runs_off_the_async_executor_thread() {
        let executor_thread = std::thread::current().id();
        let worker_thread = run_blocking(|| Ok(std::thread::current().id()))
            .await
            .expect("blocking operation succeeds");

        assert_ne!(worker_thread, executor_thread);
    }
}
