use std::future::Future;

pub(crate) fn run_on_tokio_thread(
    main: impl Future<Output = ()>,
    on_runtime_error: impl FnOnce(String),
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            on_runtime_error(format!("tokio runtime: {error}"));

            return;
        }
    };

    runtime.block_on(main);
}
