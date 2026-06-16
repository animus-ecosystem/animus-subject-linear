use animus_plugin_protocol::{PluginInfo, RpcError, PLUGIN_KIND_SUBJECT_BACKEND};
use animus_plugin_runtime::subject_plugin_with_kind_aliases;
use animus_subject_linear::backend::{CreateRequest, LinearBackend};
use animus_subject_linear::config::LinearConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let config = LinearConfig::from_env()?;
    let backend = LinearBackend::new(config)?;

    let info = PluginInfo {
        name: env!("CARGO_PKG_NAME").into(),
        version: env!("CARGO_PKG_VERSION").into(),
        plugin_kind: PLUGIN_KIND_SUBJECT_BACKEND.into(),
        description: Some(env!("CARGO_PKG_DESCRIPTION").into()),
    };

    // Base plugin: the five polled verbs in both `subject/*` and `issue/*`
    // forms (list/get/update/delete/schema). `issue/create` is NOT one of
    // them — `subject_plugin_with_kind_aliases` only wires those five — so we
    // register the create handler ourselves below. `LinearBackend` is `Clone`
    // (an `Arc` inside), so cloning per handler is cheap.
    let mut plugin = subject_plugin_with_kind_aliases(info, backend.clone(), ["issue"]);

    // Register create for BOTH the kind-prefixed form the daemon emits
    // (`issue/create`) and the canonical alias (`subject/create`).
    // `register_method` deserializes `CreateRequest` for us — the same
    // primitive the runtime uses internally for get/update.
    for method in ["issue/create", "subject/create"] {
        let backend = backend.clone();
        plugin = plugin.register_method::<CreateRequest, _, _, _>(method, move |req, _ctx| {
            let backend = backend.clone();
            async move { backend.create(req).await.map_err(RpcError::from) }
        });
    }

    // Advertise the new verbs in the manifest / `initialize` response,
    // alongside the runtime-derived defaults.
    let mut methods: Vec<String> = plugin.advertised_methods().to_vec();
    for method in ["issue/create", "subject/create"] {
        if !methods.iter().any(|m| m == method) {
            methods.push(method.to_string());
        }
    }
    plugin = plugin.methods(methods);

    plugin.run().await
}
