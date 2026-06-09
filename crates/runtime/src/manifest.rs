//! Tracked-usage manifest infrastructure.
//!
//! Each `shiku::secret!()`, `shiku::binding!()`, `shiku::listen_http!()` macro
//! call inserts a `Need` into the [`SHIKU_NEEDS`] distributed slice. The
//! linker aggregates these across the entire binary; at runtime we walk the
//! slice and produce a manifest the agent can read.
//!
//! Why it works: `linkme` uses linker-level section aggregation so each
//! macro-generated static lives in its own crate but lands in the same
//! collection at link time. No coordination needed between source files.
//!
//! Why this beats config-file declarations: there is no second source of
//! truth. Adding `shiku::secret!("FOO")` to your code is the *only* place
//! "this service needs FOO" appears. Removing the call removes the
//! declaration. No drift possible.

use linkme::distributed_slice;
use serde::{Deserialize, Serialize};

/// One thing the service needs from the platform.
///
/// Names are `'static` because they come from string literals in the macro
/// expansions — by design, dynamic names aren't supported.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Need {
    /// Secret env var by name (e.g. `"BOT_TOKEN"`).
    Secret(&'static str),
    /// Binding to another Shiku-managed service by name (e.g. `"narrator"`).
    Binding(&'static str),
    /// HTTP port allocated by the agent and exposed via `SHIKU_LISTEN_HTTP`.
    ListenHttp,
}

/// The aggregated set of needs declared across the binary. Populated by
/// macro expansions in service code; consumed by `--shiku-manifest`.
///
/// **Important**: this slice is empty by default — it only contains entries
/// for needs the binary actually declares. A service that calls no macros
/// produces an empty manifest, which is correct ("this app needs nothing
/// from the platform").
#[distributed_slice]
pub static SHIKU_NEEDS: [Need];

/// JSON-serialisable manifest dumped by `--shiku-manifest`.
///
/// We use JSON for the manifest because it's human-debuggable
/// (`./binary --shiku-manifest | jq`) and easy to parse from any language.
/// The agent runs this once after each release upload and stores the result
/// alongside the binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest schema version. Bump when the wire shape changes.
    pub version: u32,
    /// Names of secret env vars the service expects.
    pub secrets: Vec<String>,
    /// Names of bindings the service expects.
    pub bindings: Vec<String>,
    /// Whether the service expects an HTTP listen port.
    pub listen_http: bool,
}

impl Manifest {
    /// Current schema version.
    pub const VERSION: u32 = 1;

    /// Build a manifest from the linker-aggregated [`SHIKU_NEEDS`].
    ///
    /// Deduplicates and sorts each list so the manifest is deterministic
    /// (same source → same JSON byte-for-byte).
    pub fn from_static() -> Self {
        let mut secrets: Vec<String> = Vec::new();
        let mut bindings: Vec<String> = Vec::new();
        let mut listen_http = false;

        for need in SHIKU_NEEDS.iter() {
            match need {
                Need::Secret(name) => secrets.push((*name).to_string()),
                Need::Binding(name) => bindings.push((*name).to_string()),
                Need::ListenHttp => listen_http = true,
            }
        }

        secrets.sort();
        secrets.dedup();
        bindings.sort();
        bindings.dedup();

        Self {
            version: Self::VERSION,
            secrets,
            bindings,
            listen_http,
        }
    }
}

/// Handle the `--shiku-manifest` flag. If `argv` contains it, print the
/// manifest as JSON to stdout and exit. Otherwise return — the caller's
/// main continues normally.
///
/// Called from [`crate::init`] before any tracing or runtime setup so a
/// manifest probe doesn't trigger unrelated startup work (DB connections,
/// network probes, etc).
pub fn handle_manifest_flag() {
    let args = std::env::args().skip(1);
    for arg in args {
        if arg == "--shiku-manifest" {
            let manifest = Manifest::from_static();
            // Use `serde_json` for the human-readable output. Crate has it
            // already for axum.
            let json =
                serde_json::to_string(&manifest).expect("serialising manifest should never fail");
            println!("{json}");
            std::process::exit(0);
        }
    }
}

/// Hidden module exposed only for macro expansion. Lets the proc-macro
/// crate reference `linkme` and `SHIKU_NEEDS` without forcing service code
/// to depend on `linkme` directly.
#[doc(hidden)]
pub mod __macro_support {
    pub use crate::manifest::SHIKU_NEEDS;
    pub use linkme;
}
