use crate::call::{Dialplan, TransactionCookie, TrunkContext};
use crate::proxy::call::{DialplanInspector, RouteError};
use rsipstack::sip::Request as SipRequest;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static USAGE: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn usage_counter() -> &'static Mutex<HashMap<String, u64>> {
    USAGE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct NumberPoolInspector;

impl NumberPoolInspector {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NumberPoolInspector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl DialplanInspector for NumberPoolInspector {
    async fn inspect_dialplan(
        &self,
        mut dialplan: Dialplan,
        cookie: &TransactionCookie,
        _original: &SipRequest,
    ) -> Result<Dialplan, RouteError> {
        let Some(ctx) = cookie.get_extension::<TrunkContext>() else {
            return Ok(dialplan);
        };

        if ctx.did_numbers.is_empty() {
            return Ok(dialplan);
        }

        let counter = usage_counter();
        let usage = counter.lock().unwrap();
        let did = ctx
            .did_numbers
            .iter()
            .min_by_key(|d| usage.get(*d).copied().unwrap_or(0))
            .cloned()
            .expect("did_numbers is non-empty");

        drop(usage);

        if let Ok(uri) = format!("sip:{}", did).parse() {
            tracing::info!(
                trunk = %ctx.name,
                did = %did,
                "number pool: assigned least-used DID as caller"
            );
            dialplan.caller = Some(uri);
        }

        let mut usage = counter.lock().unwrap();
        *usage.entry(did).or_insert(0) += 1;

        Ok(dialplan)
    }
}
