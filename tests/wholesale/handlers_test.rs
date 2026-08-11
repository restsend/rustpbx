/// Tests for CdrQuery deserialization correctness.
///
/// The key invariant: `CdrQuery` must deserialize identically whether the
/// source is a URL query string (values are strings) or a JSON blob (values
/// are native integers/nulls). The latter case is what happens when the
/// export worker reads the `filters` column stored in `wholesale_export_tasks`.
#[cfg(test)]
mod tests {
    use rustpbx::addons::wholesale::{
        handlers::{CdrQuery, build_cdr_condition},
        models::wholesale_cdr,
    };
    use sea_orm::{DbBackend, EntityTrait, QueryFilter, QueryTrait};

    // ── JSON round-trip (simulates export-worker reading stored filters) ──────

    /// The original bug: tenant_id/carrier_id were stored as JSON integers but
    /// parse_empty_string_as_none tried to deserialize them as Option<String>,
    /// causing a type error that made unwrap_or_default() swallow all filters.
    #[test]
    fn test_json_integer_fields() {
        let json = r#"{"tenant_id":7,"carrier_id":31,"from":"2025-02-17","to":"2026-02-23","status":"any"}"#;
        let q: CdrQuery = serde_json::from_str(json).expect("must deserialize");

        assert_eq!(q.tenant_id, Some(7));
        assert_eq!(q.carrier_id, Some(31));
        assert_eq!(q.from.as_deref(), Some("2025-02-17"));
        assert_eq!(q.to.as_deref(), Some("2026-02-23"));
        assert_eq!(q.status.as_deref(), Some("any"));
    }

    /// Null values in JSON must become None.
    #[test]
    fn test_json_null_fields() {
        let json = r#"{"tenant_id":null,"carrier_id":null}"#;
        let q: CdrQuery = serde_json::from_str(json).expect("must deserialize");

        assert_eq!(q.tenant_id, None);
        assert_eq!(q.carrier_id, None);
    }

    /// Missing keys in JSON must also become None (serde default).
    #[test]
    fn test_json_missing_fields() {
        let json = r#"{}"#;
        let q: CdrQuery = serde_json::from_str(json).expect("must deserialize");

        assert_eq!(q.tenant_id, None);
        assert_eq!(q.carrier_id, None);
        assert_eq!(q.from, None);
        assert_eq!(q.to, None);
        assert_eq!(q.status, None);
    }

    /// Serializing then deserializing a CdrQuery must produce the same value.
    #[test]
    fn test_json_roundtrip() {
        let original = CdrQuery {
            from: Some("2025-01-01".to_string()),
            to: Some("2025-12-31".to_string()),
            tenant_id: Some(42),
            carrier_id: Some(9),
            caller: Some("100".to_string()),
            callee: Some("200".to_string()),
            status: Some("answered".to_string()),
            billed: Some("yes".to_string()),
            bill_id: None,
            page: Some(1),
            page_size: Some(50),
            format: None,
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let restored: CdrQuery = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.tenant_id, original.tenant_id);
        assert_eq!(restored.carrier_id, original.carrier_id);
        assert_eq!(restored.from, original.from);
        assert_eq!(restored.to, original.to);
        assert_eq!(restored.caller, original.caller);
        assert_eq!(restored.callee, original.callee);
        assert_eq!(restored.status, original.status);
        assert_eq!(restored.billed, original.billed);
        assert_eq!(restored.page, original.page);
        assert_eq!(restored.page_size, original.page_size);
        assert_eq!(restored.bill_id, original.bill_id);
    }

    // ── URL query-string deserialization (original use-case, must still work) ─

    /// When values arrive as strings (URL query params), they must still parse.
    #[test]
    fn test_query_string_integer_as_string() {
        let json = r#"{"tenant_id":"7","carrier_id":"31"}"#;
        let q: CdrQuery = serde_json::from_str(json).expect("must deserialize");

        assert_eq!(q.tenant_id, Some(7i64));
        assert_eq!(q.carrier_id, Some(31i64));
    }

    /// An empty string for an integer field must produce None.
    #[test]
    fn test_query_string_empty_as_none() {
        let json = r#"{"tenant_id":"","carrier_id":""}"#;
        let q: CdrQuery = serde_json::from_str(json).expect("must deserialize");

        assert_eq!(q.tenant_id, None);
        assert_eq!(q.carrier_id, None);
    }

    #[test]
    fn test_cdr_condition_accepts_datetime_precision_and_timezone() {
        let query = CdrQuery {
            from: Some("2026-07-27T10:11:00+08:00".to_string()),
            to: Some("2026-07-27T13:14:00Z".to_string()),
            ..Default::default()
        };

        let sql = wholesale_cdr::Entity::find()
            .filter(build_cdr_condition(&query).expect("valid RFC3339 range"))
            .build(DbBackend::Postgres)
            .to_string();

        assert!(sql.contains("2026-07-27 02:11:00"));
        assert!(sql.contains("2026-07-27 13:14:00"));
        assert!(sql.contains(r#""created_at" < '2026-07-27 13:14:00"#));
    }

    #[test]
    fn test_cdr_condition_rejects_sub_minute_precision() {
        let query = CdrQuery {
            from: Some("2026-07-27T10:11:12Z".to_string()),
            ..Default::default()
        };

        assert_eq!(
            build_cdr_condition(&query).unwrap_err(),
            "`from` must use minute precision"
        );
    }

    #[test]
    fn test_cdr_condition_rejects_date_only_values() {
        let query = CdrQuery {
            from: Some("2026-07-27".to_string()),
            ..Default::default()
        };

        assert_eq!(
            build_cdr_condition(&query).unwrap_err(),
            "`from` must be an RFC3339 datetime"
        );
    }

    #[test]
    fn test_cdr_condition_rejects_reversed_range() {
        let query = CdrQuery {
            from: Some("2026-07-27T11:00:00Z".to_string()),
            to: Some("2026-07-27T10:00:00Z".to_string()),
            ..Default::default()
        };

        assert_eq!(
            build_cdr_condition(&query).unwrap_err(),
            "`from` must be earlier than `to`"
        );
    }

    #[test]
    fn test_cdr_condition_includes_all_record_selection_filters() {
        let query = CdrQuery {
            tenant_id: Some(7),
            carrier_id: Some(31),
            caller: Some("1001".to_string()),
            callee: Some("2002".to_string()),
            status: Some("answered".to_string()),
            billed: Some("no".to_string()),
            ..Default::default()
        };

        let sql = wholesale_cdr::Entity::find()
            .filter(build_cdr_condition(&query).expect("valid filters"))
            .build(DbBackend::Postgres)
            .to_string();

        assert!(sql.contains(r#""tenant_id" = 7"#));
        assert!(sql.contains(r#""carrier_id" = 31"#));
        assert!(sql.contains(r#""caller" = '1001'"#));
        assert!(sql.contains(r#""callee" = '2002'"#));
        assert!(sql.contains(r#""status" IN ('answered', 'completed')"#));
        assert!(sql.contains(r#""bill_id" IS NULL"#));
    }

    #[test]
    fn test_bill_id_does_not_suppress_other_explicit_filters() {
        let query = CdrQuery {
            bill_id: Some(123),
            tenant_id: Some(7),
            status: Some("answered".to_string()),
            ..Default::default()
        };

        let sql = wholesale_cdr::Entity::find()
            .filter(build_cdr_condition(&query).expect("valid filters"))
            .build(DbBackend::Postgres)
            .to_string();

        assert!(sql.contains(r#""bill_id" = 123"#));
        assert!(sql.contains(r#""tenant_id" = 7"#));
        assert!(sql.contains(r#""status" IN ('answered', 'completed')"#));
        assert!(!sql.contains(r#""created_at" >= "#));
    }

    #[test]
    fn test_cdr_condition_has_no_hidden_datetime_default() {
        let sql = wholesale_cdr::Entity::find()
            .filter(build_cdr_condition(&CdrQuery::default()).expect("empty filters are valid"))
            .build(DbBackend::Postgres)
            .to_string();

        assert!(!sql.contains(r#""created_at" >= "#));
        assert!(!sql.contains(r#""created_at" < "#));
    }

    // ── bill_id field ─────────────────────────────────────────────────────────

    #[test]
    fn test_bill_id_integer() {
        let json = r#"{"bill_id":123}"#;
        let q: CdrQuery = serde_json::from_str(json).expect("must deserialize");
        assert_eq!(q.bill_id, Some(123i64));
    }

    #[test]
    fn test_bill_id_null() {
        let json = r#"{"bill_id":null}"#;
        let q: CdrQuery = serde_json::from_str(json).expect("must deserialize");
        assert_eq!(q.bill_id, None);
    }

    #[test]
    fn test_bill_id_empty_string() {
        let json = r#"{"bill_id":""}"#;
        let q: CdrQuery = serde_json::from_str(json).expect("must deserialize");
        assert_eq!(q.bill_id, None);
    }
}

