//! The built-in `Memory*` implementations run through the same
//! conformance suites a third-party adapter would (§15.3) — nothing is
//! special-cased for them.

use std::collections::BTreeMap;
use std::time::Duration;

use fulmen_core::adapters::{
    AdapterError, MemoryCacheStore, MemoryJobQueue, MemoryRateLimitStore, MemorySessionStore,
    SpecValue, StoreResult, TypeRef, Validator,
};
use fulmen_testkit::{cache, jobs, ratelimit, session, validator};

#[test]
fn memory_session_store_conforms() {
    session::suite(&MemorySessionStore::new());
}

#[test]
fn memory_cache_store_conforms() {
    cache::suite(&MemoryCacheStore::new());
}

#[test]
fn memory_rate_limit_store_conforms() {
    ratelimit::suite(&MemoryRateLimitStore::new());
}

#[test]
fn memory_job_queue_conforms() {
    jobs::suite(&MemoryJobQueue::with_limits(
        Duration::from_millis(40),
        1_024,
    ));
}

/// A `Validator` stand-in: supports `builtins.int` and compiles it to
/// the `{"kind": "int"}` spec leaf. The Rust trait has no built-in
/// implementation (§15.2's default is the Python-side native
/// compiler), so the suite is exercised against this stub.
struct IntValidator;

impl Validator for IntValidator {
    fn supports(&self, tp: &TypeRef) -> bool {
        tp.module == "builtins" && tp.qualname == "int" && tp.args.is_empty()
    }

    fn compile(&self, tp: &TypeRef) -> StoreResult<SpecValue> {
        if !self.supports(tp) {
            return Err(AdapterError::Unsupported(format!("{tp}")));
        }
        Ok(SpecValue::Map(BTreeMap::from([(
            "kind".to_string(),
            SpecValue::Str("int".to_string()),
        )])))
    }

    fn json_schema(&self, tp: &TypeRef) -> StoreResult<SpecValue> {
        if !self.supports(tp) {
            return Err(AdapterError::Unsupported(format!("{tp}")));
        }
        Ok(SpecValue::Map(BTreeMap::from([(
            "type".to_string(),
            SpecValue::Str("integer".to_string()),
        )])))
    }
}

#[test]
fn validator_suite_runs() {
    validator::suite(
        &IntValidator,
        &TypeRef::new("builtins", "int"),
        &TypeRef::new("builtins", "str"),
    );
}
