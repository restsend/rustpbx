/// CPS / Concurrent-call limit enforcement 测试
///
/// 分两层：
/// 1. `state_*` 系列：纯内存单元测试，验证 tenant runtime limiter 逻辑
/// 2. `route_*` 系列：路由层集成测试，验证 `route_wholesale()` 正确返回 503
#[cfg(test)]
mod tests {
    use rustpbx::addons::wholesale::{
        data::{RateConfig, RateDeckConfig, RoutingSnapshot, Tenant, WholesaleState},
        migration::Migrator as WholesaleMigrator,
        models::{rate, rate_deck, routing_profile, routing_profile_item, tenant, tenant_trunk},
        route::{WholesaleBillingContext, WholesaleRouteInvite},
    };
    use rustpbx::call::{
        DialDirection, RouteInvite, TransactionCookie, TrunkContext,
        concurrent_call_limiter::{
            ConcurrentCallLimitExceeded, ConcurrentCallLimiter, ConcurrentCallPermit,
        },
        cps_limiter::{CpsLimitExceeded, CpsLimiter},
    };
    use rustpbx::config::RouteResult;
    use rustpbx::models::{migration::Migrator as MainMigrator, sip_trunk};
    use chrono::Utc;
    use rsipstack::dialog::invitation::InviteOption;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use std::time::Duration;

    // ─── Helpers ──────────────────────────────────────────────────────────────
    const TEST_SOURCE_IP: &str = "127.0.0.1";

    fn test_source_allowed_ips() -> serde_json::Value {
        serde_json::json!([TEST_SOURCE_IP])
    }

    async fn setup_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        MainMigrator::up(&db, None).await.expect("main migrations");
        WholesaleMigrator::up(&db, None)
            .await
            .expect("wholesale migrations");
        db
    }

    /// 建立最小化的 WholesaleRouteInvite，租户可配置 max_concurrent / max_cps。
    /// 返回 (route_invite, tenant_id, inbound_trunk_id)。
    async fn setup_route_invite_with_limits(
        db: DatabaseConnection,
        state: WholesaleState,
        max_concurrent: Option<i32>,
        max_cps: Option<i32>,
    ) -> (WholesaleRouteInvite, i64, i64) {
        let carrier_trunk = sip_trunk::ActiveModel {
            name: Set("RL Carrier".to_string()),
            sip_server: Set(Some("10.0.0.1:5060".to_string())),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create outbound trunk");

        let inbound = sip_trunk::ActiveModel {
            name: Set("RL Source".to_string()),
            allowed_ips: Set(Some(test_source_allowed_ips())),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create source trunk");

        let profile = routing_profile::ActiveModel {
            name: Set("RL Profile".to_string()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create profile");

        routing_profile_item::ActiveModel {
            profile_id: Set(profile.id),
            sip_trunk_id: Set(carrier_trunk.id),
            priority: Set(0),
            weight: Set(100),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create profile item");

        let deck = rate_deck::ActiveModel {
            name: Set("RL Deck".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create deck");

        rate::ActiveModel {
            deck_id: Set(deck.id),
            prefix: Set("1".to_string()),
            rate: Set(0.01),
            min_duration: Set(60),
            increment: Set(60),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create rate");

        let tenant = tenant::ActiveModel {
            name: Set("RL Tenant".to_string()),
            balance: Set(1000.0),
            routing_profile_id: Set(Some(profile.id)),
            rate_deck_id: Set(Some(deck.id)),
            max_concurrent: Set(max_concurrent),
            max_cps: Set(max_cps),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create tenant");

        tenant_trunk::ActiveModel {
            tenant_id: Set(tenant.id),
            sip_trunk_id: Set(inbound.id),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("link tenant trunk");

        let state = Arc::new(state);
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "RL Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.01,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
        crate::wholesale_helpers::load_runtime_routing_profiles(&state, &db).await;

        let route_invite = WholesaleRouteInvite {
            db: db.clone(),
            state,
        };

        (route_invite, tenant.id, inbound.id)
    }

    fn make_cookie(inbound_trunk_id: i64) -> TransactionCookie {
        let cookie = TransactionCookie::default();
        cookie.insert_extension(TrunkContext {
            id: Some(inbound_trunk_id),
            name: String::new(),
            did_numbers: vec![],
        });
        cookie
    }

    /// 构造测试 INVITE 请求与 InviteOption。
    fn make_invite(callee: &str, caller: &str) -> (rsipstack::sip::Request, InviteOption) {
        let invite_uri = rsipstack::sip::Uri::try_from(format!("sip:{}@10.0.0.1", callee)).unwrap();
        let from_uri = rsipstack::sip::Uri::try_from(format!("sip:{}@10.0.0.1", caller)).unwrap();
        let to_uri = rsipstack::sip::Uri::try_from(format!("sip:{}@10.0.0.1", callee)).unwrap();
        let req = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: invite_uri.clone(),
            headers: vec![
                rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                    "<{}>;tag=rl",
                    from_uri
                ))),
                rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!(
                    "<{}>",
                    to_uri
                ))),
                rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("rl-call-id")),
                rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
                rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                    "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bKrl",
                )),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: Default::default(),
        };
        let opt = InviteOption {
            callee: invite_uri,
            caller: from_uri,
            ..Default::default()
        };
        (req, opt)
    }

    fn test_tenant_limit_model(tenant_id: i64, max_concurrent: Option<i32>) -> tenant::Model {
        tenant::Model {
            id: tenant_id,
            name: format!("Tenant {}", tenant_id),
            contact_name: None,
            contact_email: None,
            contact_phone: None,
            balance: 0.0,
            credit_limit: 0.0,
            currency: "USD".to_string(),
            max_concurrent,
            max_cps: None,
            routing_profile_id: None,
            rate_deck_id: None,
            billing_cycle: None,
            bill_recipient_name: None,
            enable_recording: false,
            bypass_media: false,
            duration_rounding: "floor".to_string(),
            lcr_enabled: false,
            remark: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn test_tenant_limit_model_with_cps(
        tenant_id: i64,
        max_concurrent: Option<i32>,
        max_cps: Option<i32>,
    ) -> tenant::Model {
        let mut model = test_tenant_limit_model(tenant_id, max_concurrent);
        model.max_cps = max_cps;
        model
    }

    fn cache_test_tenant_limit(
        state: &WholesaleState,
        tenant_id: i64,
        max_concurrent: Option<i32>,
    ) {
        load_test_tenant_snapshot(
            state,
            vec![test_tenant_limit_model(tenant_id, max_concurrent)],
        );
    }

    fn load_test_tenant_snapshot(state: &WholesaleState, tenants: Vec<tenant::Model>) {
        let tenants = tenants
            .into_iter()
            .map(|tenant_model| {
                let max_cps = tenant_model.max_cps;
                let max_concurrent = tenant_model.max_concurrent;
                Tenant {
                    id: tenant_model.id,
                    name: tenant_model.name,
                    routing_profile_id: tenant_model.routing_profile_id,
                    rate_deck_id: tenant_model.rate_deck_id,
                    enable_recording: tenant_model.enable_recording,
                    bypass_media: tenant_model.bypass_media,
                    lcr_enabled: tenant_model.lcr_enabled,
                    rate_deck: None,
                    route_table: None,
                    cps_limiter: max_cps
                        .and_then(|limit| u32::try_from(limit).ok())
                        .and_then(NonZeroU32::new)
                        .map(CpsLimiter::new),
                    concurrent_call_limiter: max_concurrent
                        .and_then(|limit| u32::try_from(limit).ok())
                        .filter(|limit| *limit > 0)
                        .map(ConcurrentCallLimiter::new),
                }
            })
            .collect();
        let mut snapshot = RoutingSnapshot::default();
        snapshot.tenants = tenants;
        state.routing.store(Arc::new(snapshot));
    }

    fn try_acquire_test_tenant_concurrent(
        state: &WholesaleState,
        tenant_id: i64,
    ) -> std::result::Result<Option<ConcurrentCallPermit>, ConcurrentCallLimitExceeded> {
        let snapshot = state.routing.load();
        let Some(tenant) = snapshot
            .tenants
            .iter()
            .find(|tenant| tenant.id == tenant_id)
        else {
            return Ok(None);
        };

        let Some(limiter) = tenant.concurrent_call_limiter.as_ref() else {
            return Ok(None);
        };
        limiter.try_acquire().map(Some)
    }

    fn test_concurrent_count(
        state: &WholesaleState,
        tenant_id: i64,
        max_concurrent: Option<i32>,
    ) -> i32 {
        let snapshot = state.routing.load();
        let Some(tenant) = snapshot
            .tenants
            .iter()
            .find(|tenant| tenant.id == tenant_id)
        else {
            return 0;
        };
        let _ = max_concurrent;
        tenant
            .concurrent_call_limiter
            .as_ref()
            .map(|limiter| i32::try_from(limiter.current()).unwrap_or(i32::MAX))
            .unwrap_or(0)
    }

    fn try_acquire_test_tenant_cps(
        state: &WholesaleState,
        tenant_id: i64,
    ) -> std::result::Result<Option<u32>, CpsLimitExceeded> {
        let snapshot = state.routing.load();
        let Some(tenant) = snapshot
            .tenants
            .iter()
            .find(|tenant| tenant.id == tenant_id)
        else {
            return Ok(None);
        };
        let Some(limiter) = tenant.cps_limiter.as_ref() else {
            return Ok(None);
        };

        match limiter.try_acquire() {
            Ok(()) => Ok(Some(limiter.current_count())),
            Err(rejection) => Err(rejection),
        }
    }

    fn test_tenant_cps_count(state: &WholesaleState, tenant_id: i64) -> u32 {
        let snapshot = state.routing.load();
        snapshot
            .tenants
            .iter()
            .find(|tenant| tenant.id == tenant_id)
            .and_then(|tenant| tenant.cps_limiter.as_ref())
            .map(CpsLimiter::current_count)
            .unwrap_or(0)
    }

    // ─── 纯内存单元测试：WholesaleState 计数器 ────────────────────────────────

    /// 无限制时不创建 semaphore，也不返回 permit。
    #[test]
    fn state_concurrent_no_limit() {
        let state = WholesaleState::new();
        for _ in 0..100 {
            assert!(
                try_acquire_test_tenant_concurrent(&state, 1)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(test_concurrent_count(&state, 1, None), 0);
    }

    /// 达到上限后返回 Err。
    #[test]
    fn state_concurrent_limit_enforced() {
        let state = WholesaleState::new();
        let tid = 42i64;
        cache_test_tenant_limit(&state, tid, Some(3));
        let mut permits = Vec::new();
        // 填满
        for _ in 0..3 {
            permits.push(
                try_acquire_test_tenant_concurrent(&state, tid)
                    .unwrap()
                    .unwrap(),
            );
        }
        // 第 4 次应被拒绝
        let err = try_acquire_test_tenant_concurrent(&state, tid);
        assert!(err.is_err(), "Err should indicate acquire rejection");
        // 计数不应因被拒绝而增加
        assert_eq!(test_concurrent_count(&state, tid, Some(3)), 3);
        drop(permits);
    }

    /// 提高租户并发上限时，通过重建 semaphore 生效。
    #[test]
    fn state_concurrent_limit_increase_rebuilds_semaphore() {
        let state = WholesaleState::new();
        let tid = 43i64;
        cache_test_tenant_limit(&state, tid, Some(1));
        let _old = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();

        assert!(try_acquire_test_tenant_concurrent(&state, tid).is_err());

        cache_test_tenant_limit(&state, tid, Some(2));
        assert_eq!(test_concurrent_count(&state, tid, Some(2)), 0);

        let first = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        let second = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();

        assert_eq!(test_concurrent_count(&state, tid, Some(2)), 2);
        drop(first);
        drop(second);
    }

    /// 降低租户并发上限时，通过重建 semaphore 生效，旧 permit 与新计数分离。
    #[test]
    fn state_concurrent_limit_decrease_rebuilds_semaphore() {
        let state = WholesaleState::new();
        let tid = 44i64;
        cache_test_tenant_limit(&state, tid, Some(3));
        let _first = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        let _second = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        let _third = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();

        assert_eq!(test_concurrent_count(&state, tid, Some(3)), 3);

        cache_test_tenant_limit(&state, tid, Some(1));
        assert_eq!(test_concurrent_count(&state, tid, Some(1)), 0);

        let next = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        assert_eq!(test_concurrent_count(&state, tid, Some(1)), 1);
        assert!(try_acquire_test_tenant_concurrent(&state, tid).is_err());
        drop(next);
    }

    /// permit drop 正确释放 semaphore 槽位。
    #[test]
    fn state_concurrent_release() {
        let state = WholesaleState::new();
        let tid = 7i64;
        cache_test_tenant_limit(&state, tid, Some(2));
        let first = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        let second = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        assert_eq!(test_concurrent_count(&state, tid, Some(2)), 2);

        drop(first);
        assert_eq!(test_concurrent_count(&state, tid, Some(2)), 1);

        drop(second);
        assert_eq!(test_concurrent_count(&state, tid, Some(2)), 0);
    }

    /// 路由层拿到的 permit drop 后释放槽位。
    #[test]
    fn state_concurrent_permit_drop_releases_slot() {
        let state = WholesaleState::new();
        let tid = 8i64;
        cache_test_tenant_limit(&state, tid, Some(1));
        let permit = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        assert_eq!(test_concurrent_count(&state, tid, Some(1)), 1);

        drop(permit);
        assert_eq!(test_concurrent_count(&state, tid, Some(1)), 0);
    }

    /// reload tenant 后旧 permit 释放时，不应扣减 reload 后新建的 semaphore。
    #[test]
    fn state_concurrent_old_permit_does_not_release_new_counter_after_reload() {
        let state = WholesaleState::new();
        let tid = 9i64;
        cache_test_tenant_limit(&state, tid, Some(1));
        let old_permit = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        assert_eq!(test_concurrent_count(&state, tid, Some(1)), 1);

        cache_test_tenant_limit(&state, tid, Some(1));
        assert_eq!(test_concurrent_count(&state, tid, Some(1)), 0);

        let new_permit = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        assert_eq!(test_concurrent_count(&state, tid, Some(1)), 1);

        drop(old_permit);
        assert_eq!(
            test_concurrent_count(&state, tid, Some(1)),
            1,
            "old permit must not decrement the new post-reload counter"
        );

        drop(new_permit);
        assert_eq!(test_concurrent_count(&state, tid, Some(1)), 0);
    }

    /// release 之后可以重新 acquire。
    #[test]
    fn state_concurrent_release_then_acquire() {
        let state = WholesaleState::new();
        let tid = 99i64;
        cache_test_tenant_limit(&state, tid, Some(1));
        let permit = try_acquire_test_tenant_concurrent(&state, tid)
            .unwrap()
            .unwrap();
        // 此时已满
        assert!(try_acquire_test_tenant_concurrent(&state, tid).is_err());
        // 释放后再 acquire 应成功
        drop(permit);
        assert!(
            try_acquire_test_tenant_concurrent(&state, tid)
                .unwrap()
                .is_some()
        );
    }

    /// 不同租户的计数器相互独立。
    #[test]
    fn state_concurrent_multi_tenant_isolation() {
        let state = WholesaleState::new();
        load_test_tenant_snapshot(
            &state,
            vec![
                test_tenant_limit_model(1, Some(2)),
                test_tenant_limit_model(2, Some(5)),
            ],
        );
        let mut tenant_a_permits = Vec::new();
        // 租户 A 限制 2
        tenant_a_permits.push(
            try_acquire_test_tenant_concurrent(&state, 1)
                .unwrap()
                .unwrap(),
        );
        tenant_a_permits.push(
            try_acquire_test_tenant_concurrent(&state, 1)
                .unwrap()
                .unwrap(),
        );
        assert!(try_acquire_test_tenant_concurrent(&state, 1).is_err());

        // 租户 B 限制 5，不受 A 影响
        let mut tenant_b_permits = Vec::new();
        for _ in 0..5 {
            tenant_b_permits.push(
                try_acquire_test_tenant_concurrent(&state, 2)
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(test_concurrent_count(&state, 1, Some(2)), 2);
        assert_eq!(test_concurrent_count(&state, 2, Some(5)), 5);
        drop(tenant_a_permits);
        drop(tenant_b_permits);
    }

    /// 无 CPS 限制时始终返回 Ok。
    #[test]
    fn state_cps_no_limit() {
        let state = WholesaleState::new();
        load_test_tenant_snapshot(&state, vec![test_tenant_limit_model(1, None)]);
        for _ in 0..50 {
            assert_eq!(try_acquire_test_tenant_cps(&state, 1).unwrap(), None);
        }
    }

    /// GCRA burst 用完后返回 Err。
    #[test]
    fn state_cps_limit_enforced() {
        let state = WholesaleState::new();
        let tid = 10i64;
        load_test_tenant_snapshot(
            &state,
            vec![test_tenant_limit_model_with_cps(tid, None, Some(3))],
        );
        // 允许 3 CPS
        for i in 0..3 {
            assert!(
                try_acquire_test_tenant_cps(&state, tid).is_ok(),
                "call {} should pass",
                i + 1
            );
        }
        // 第 4 次应被拒绝
        let err = try_acquire_test_tenant_cps(&state, tid);
        assert!(err.is_err(), "4th call should be rejected");
        assert_eq!(
            err.unwrap_err(),
            CpsLimitExceeded { limit: 3 },
            "Err contains the configured CPS limit"
        );
    }

    /// 等待 GCRA refill 后，CPS limiter 应允许新调用。
    #[tokio::test]
    async fn state_cps_window_expiry() {
        let state = WholesaleState::new();
        let tid = 20i64;
        load_test_tenant_snapshot(
            &state,
            vec![test_tenant_limit_model_with_cps(tid, None, Some(1))],
        );
        // 打满 1 CPS
        try_acquire_test_tenant_cps(&state, tid).unwrap();
        assert!(try_acquire_test_tenant_cps(&state, tid).is_err());

        // 等待 refill
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // 已 refill，应能再次通过
        assert!(
            try_acquire_test_tenant_cps(&state, tid).is_ok(),
            "CPS limiter should have refilled"
        );
    }

    /// 不同租户的 CPS limiter 相互独立。
    #[test]
    fn state_cps_multi_tenant_isolation() {
        let state = WholesaleState::new();
        load_test_tenant_snapshot(
            &state,
            vec![
                test_tenant_limit_model_with_cps(1, None, Some(1)),
                test_tenant_limit_model_with_cps(2, None, Some(5)),
            ],
        );
        // 租户 A 限制 1
        try_acquire_test_tenant_cps(&state, 1).unwrap();
        assert!(try_acquire_test_tenant_cps(&state, 1).is_err());

        // 租户 B 不受影响
        assert!(try_acquire_test_tenant_cps(&state, 2).is_ok());
    }

    /// tenant limiter 返回当前 GCRA 压力估算值。
    #[test]
    fn state_cps_get_count() {
        let state = WholesaleState::new();
        let tid = 30i64;
        load_test_tenant_snapshot(
            &state,
            vec![test_tenant_limit_model_with_cps(tid, None, Some(10))],
        );
        assert_eq!(test_tenant_cps_count(&state, tid), 0);
        try_acquire_test_tenant_cps(&state, tid).unwrap();
        try_acquire_test_tenant_cps(&state, tid).unwrap();
        assert_eq!(test_tenant_cps_count(&state, tid), 2);
    }

    // ─── 路由层集成测试 ───────────────────────────────────────────────────────

    /// 未设置任何限制时，路由正常返回 Forward。
    #[tokio::test]
    async fn route_no_limit_passes() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, _tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db, state, None, None).await;
        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        assert!(
            matches!(result, RouteResult::Forward(_, _)),
            "no limits: should Forward, but got Abort or other variant"
        );
    }

    #[tokio::test]
    async fn route_low_balance_does_not_set_max_duration() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db, state, None, None).await;

        tenant::ActiveModel {
            id: Set(tenant_id),
            balance: Set(0.005),
            credit_limit: Set(0.0),
            ..Default::default()
        }
        .update(&ri.db)
        .await
        .expect("set tenant balance below one minute of calling");

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");
        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        let RouteResult::Forward(_, Some(hints)) = result else {
            panic!("expected Forward with hints");
        };
        assert_eq!(hints.max_duration, None);
    }

    /// 并发数限制为 1，第 1 次 OK，第 2 次应返回 503 Service Unavailable。
    #[tokio::test]
    async fn route_concurrent_limit_rejected() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db.clone(), state, Some(1), None).await;

        // 手动占用一个槽（模拟第 1 路通话已经接通，还未释放）
        let _held = try_acquire_test_tenant_concurrent(&ri.state, tenant_id)
            .unwrap()
            .unwrap();

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        match result {
            RouteResult::Abort(code, reason) => {
                assert_eq!(
                    code,
                    rsipstack::sip::StatusCode::ServiceUnavailable,
                    "concurrent limit should return 503"
                );
                let reason = reason.unwrap_or_default();
                assert!(
                    reason.contains("Concurrent call limit exceeded"),
                    "reason should mention concurrent limit, got: {}",
                    reason
                );
            }
            _ => panic!("Expected Abort(503 ServiceUnavailable), but got Forward or other variant"),
        }
    }

    /// preview_route 失败时也必须设置 wholesale billing context，失败 CDR 才能归到 wholesale。
    #[tokio::test]
    async fn preview_concurrent_limit_sets_tenant_context() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db.clone(), state, Some(1), None).await;

        let _held = try_acquire_test_tenant_concurrent(&ri.state, tenant_id)
            .unwrap()
            .unwrap();

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        let result = ri
            .preview_route(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        assert!(matches!(
            result,
            RouteResult::Abort(rsipstack::sip::StatusCode::ServiceUnavailable, _)
        ));
        let billing_ctx = cookie
            .get_extension::<WholesaleBillingContext>()
            .expect("billing context in cookie");
        assert_eq!(billing_ctx.tenant_id, tenant_id);
        assert_eq!(billing_ctx.carrier_id, None);
        assert_eq!(
            cookie
                .get_extension::<TrunkContext>()
                .and_then(|ctx| ctx.id),
            Some(inbound_trunk_id)
        );
    }

    /// Tenant 识别成功但没有 routing profile 时，也要留下 wholesale billing context。
    #[tokio::test]
    async fn identified_tenant_without_profile_sets_billing_context() {
        let db = setup_db().await;
        let inbound = sip_trunk::ActiveModel {
            name: Set("No Profile Source".to_string()),
            allowed_ips: Set(Some(test_source_allowed_ips())),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create source trunk");

        let tenant = tenant::ActiveModel {
            name: Set("No Profile Tenant".to_string()),
            balance: Set(1000.0),
            routing_profile_id: Set(None),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create tenant");

        tenant_trunk::ActiveModel {
            tenant_id: Set(tenant.id),
            sip_trunk_id: Set(inbound.id),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("link tenant trunk");

        let ri = WholesaleRouteInvite {
            db: db.clone(),
            state: Arc::new(WholesaleState::new()),
        };
        crate::wholesale_helpers::load_runtime_routing_profiles(&ri.state, &db).await;

        for use_preview in [false, true] {
            let cookie = make_cookie(inbound.id);
            let (req, opt) = make_invite("123456", "caller");
            let result = if use_preview {
                ri.preview_route(opt, &req, &DialDirection::Outbound, &cookie)
                    .await
                    .unwrap()
            } else {
                ri.route_invite(opt, &req, &DialDirection::Outbound, &cookie)
                    .await
                    .unwrap()
            };

            assert!(matches!(
                result,
                RouteResult::Abort(rsipstack::sip::StatusCode::ServiceUnavailable, _)
            ));
            let billing_ctx = cookie
                .get_extension::<WholesaleBillingContext>()
                .expect("billing context in cookie");
            assert_eq!(billing_ctx.tenant_id, tenant.id);
            assert_eq!(billing_ctx.carrier_id, None);
        }
    }

    /// CPS 限制为 1，同一秒第 2 次调用应返回 503 Service Unavailable。
    #[tokio::test]
    async fn route_cps_limit_rejected() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db, state, None, Some(1)).await;

        // 预先占用 CPS burst，触发下一次路由时的 CPS 限制
        try_acquire_test_tenant_cps(&ri.state, tenant_id)
            .unwrap()
            .unwrap();

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        match result {
            RouteResult::Abort(code, reason) => {
                assert_eq!(
                    code,
                    rsipstack::sip::StatusCode::ServiceUnavailable,
                    "CPS limit should return 503"
                );
                let reason = reason.unwrap_or_default();
                assert!(
                    reason.contains("CPS limit exceeded"),
                    "reason should mention CPS limit, got: {}",
                    reason
                );
            }
            _ => panic!("Expected Abort(503 ServiceUnavailable), but got Forward or other variant"),
        }
    }

    /// 成功路由后，并发计数器应增加 1，并通过返回的 hold 释放。
    #[tokio::test]
    async fn route_successful_attaches_concurrent_hold() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db, state, Some(5), None).await;

        assert_eq!(test_concurrent_count(&ri.state, tenant_id, Some(5)), 0);

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        match result {
            RouteResult::Forward(_, Some(hints)) => {
                assert_eq!(
                    test_concurrent_count(&ri.state, tenant_id, Some(5)),
                    1,
                    "concurrent count should be 1 after successful route"
                );
                assert!(
                    hints.concurrent_call_lease.len() == 1,
                    "successful wholesale route should return one tenant CC hold"
                );
                hints.concurrent_call_lease.release_all();
                assert_eq!(
                    test_concurrent_count(&ri.state, tenant_id, Some(5)),
                    0,
                    "releasing the returned hold should clear the tenant CC count"
                );
            }
            RouteResult::Forward(_, None) => panic!("expected Forward with hints"),
            _ => panic!("expected Forward"),
        }
    }

    /// 路由失败且未返回 dialplan 时，不应占用租户并发槽。
    #[tokio::test]
    async fn route_no_sell_rate_does_not_hold_concurrent() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db, state, Some(1), None).await;

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("999999", "caller");

        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        match result {
            RouteResult::Abort(code, reason) => {
                assert_eq!(code, rsipstack::sip::StatusCode::ServiceUnavailable);
                assert!(
                    reason
                        .unwrap_or_default()
                        .contains("No matching sell rate deck"),
                    "expected no matching sell rate deck"
                );
            }
            _ => panic!("expected Abort"),
        }
        assert_eq!(
            test_concurrent_count(&ri.state, tenant_id, Some(1)),
            0,
            "failed wholesale route should not acquire tenant CC"
        );
    }

    #[tokio::test]
    async fn route_missing_runtime_sell_rate_deck_returns_specific_reason() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db.clone(), state, Some(1), None).await;

        let unloaded_deck = rate_deck::ActiveModel {
            name: Set("Unloaded Sell Deck".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create unloaded deck");

        tenant::ActiveModel {
            id: Set(tenant_id),
            rate_deck_id: Set(Some(unloaded_deck.id)),
            ..Default::default()
        }
        .update(&db)
        .await
        .expect("point tenant to unloaded deck");
        crate::wholesale_helpers::load_runtime_routing_profiles(&ri.state, &db).await;

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        match result {
            RouteResult::Abort(code, reason) => {
                assert_eq!(code, rsipstack::sip::StatusCode::ServiceUnavailable);
                assert!(
                    reason
                        .unwrap_or_default()
                        .contains("No matching sell rate deck"),
                    "expected no matching sell rate deck"
                );
            }
            _ => panic!("expected Abort"),
        }
    }

    #[tokio::test]
    async fn route_insufficient_funds_keeps_tenant_billing_context() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db.clone(), state, Some(1), None).await;

        tenant::ActiveModel {
            id: Set(tenant_id),
            balance: Set(0.0),
            credit_limit: Set(0.0),
            ..Default::default()
        }
        .update(&db)
        .await
        .expect("set tenant balance to zero");

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        match result {
            RouteResult::Abort(code, reason) => {
                assert_eq!(code, rsipstack::sip::StatusCode::PaymentRequired);
                assert_eq!(reason.as_deref(), Some("Insufficient funds"));
            }
            _ => panic!("expected insufficient funds abort"),
        }

        let billing_ctx = cookie
            .get_extension::<WholesaleBillingContext>()
            .expect("billing context in cookie");

        assert_eq!(billing_ctx.tenant_id, tenant_id);
        assert_eq!(billing_ctx.carrier_id, None);
    }

    /// 成功路由后，CPS limiter 应记录 1 路压力。
    #[tokio::test]
    async fn route_successful_records_cps() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db, state, None, Some(1)).await;

        assert_eq!(test_tenant_cps_count(&ri.state, tenant_id), 0);

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        ri.route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        assert_eq!(
            test_tenant_cps_count(&ri.state, tenant_id),
            1,
            "CPS limiter should record 1 call after successful route"
        );
    }

    /// 并发和 CPS 同时设限，CPS 先触发（CPS 检查在并发检查之前）。
    #[tokio::test]
    async fn route_cps_checked_before_concurrent() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db, state, Some(5), Some(1)).await;

        // 预置 CPS limiter 已满
        try_acquire_test_tenant_cps(&ri.state, tenant_id)
            .unwrap()
            .unwrap();

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");

        let result = ri
            .route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        // 应因 CPS 被 503 拒绝，而不是 486
        match result {
            RouteResult::Abort(code, _) => {
                assert_eq!(
                    code,
                    rsipstack::sip::StatusCode::ServiceUnavailable,
                    "CPS 检查应先于并发检查"
                );
            }
            _ => panic!("Expected Abort(503 ServiceUnavailable), but got Forward or other variant"),
        }

        // 并发计数器不应被修改（被 CPS 拒绝，未到并发检查）
        assert_eq!(test_concurrent_count(&ri.state, tenant_id, Some(5)), 0);
    }

    /// 被拒绝（CPS / 并发）时，并发计数器不应增加。
    #[tokio::test]
    async fn route_rejected_does_not_increment_concurrent() {
        let db = setup_db().await;
        let state = WholesaleState::new();
        let (ri, tenant_id, inbound_trunk_id) =
            setup_route_invite_with_limits(db, state, Some(1), None).await;

        // 占满并发槽
        let _held = try_acquire_test_tenant_concurrent(&ri.state, tenant_id)
            .unwrap()
            .unwrap();
        assert_eq!(test_concurrent_count(&ri.state, tenant_id, Some(1)), 1);

        let cookie = make_cookie(inbound_trunk_id);
        let (req, opt) = make_invite("123456", "caller");
        ri.route_invite(opt, &req, &DialDirection::Outbound, &cookie)
            .await
            .unwrap();

        // 被拒绝后计数器仍为 1，没有泄漏
        assert_eq!(
            test_concurrent_count(&ri.state, tenant_id, Some(1)),
            1,
            "rejected call must not increment concurrent counter"
        );
    }
}

