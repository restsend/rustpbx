use chrono::Utc;
use rustpbx::addons::wholesale::data::{RateDeckConfig, RoutingProfileConfig, WholesaleState};
use rustpbx::addons::wholesale::models::{
    rate, rate_deck, routing_profile, routing_profile_item, wholesale_trunk_config,
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};

pub async fn insert_runtime_rate_deck(db: &DatabaseConnection, deck: RateDeckConfig) {
    let deck_type = match deck.r#type.as_str() {
        "buy" => rate_deck::RateDeckType::Buy,
        _ => rate_deck::RateDeckType::Sell,
    };
    let now = Utc::now();
    if let Some(existing) = rate_deck::Entity::find_by_id(deck.id)
        .one(db)
        .await
        .expect("load runtime rate deck")
    {
        let mut active: rate_deck::ActiveModel = existing.into();
        active.name = Set(deck.name.clone());
        active.description = Set(deck.description.clone());
        active.r#type = Set(deck_type);
        active.updated_at = Set(now);
        active.update(db).await.expect("update runtime rate deck");
    } else {
        rate_deck::ActiveModel {
            id: Set(deck.id),
            name: Set(deck.name.clone()),
            description: Set(deck.description.clone()),
            r#type: Set(deck_type),
            remark: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .expect("insert runtime rate deck");
    }

    rate::Entity::delete_many()
        .filter(rate::Column::DeckId.eq(deck.id))
        .exec(db)
        .await
        .expect("clear runtime rates");
    for deck_rate in deck.rates {
        rate::ActiveModel {
            deck_id: Set(deck.id),
            prefix: Set(deck_rate.prefix),
            match_caller_prefix: Set(deck_rate.match_caller_prefix),
            rate: Set(deck_rate.rate),
            min_duration: Set(deck_rate.min_duration),
            increment: Set(deck_rate.increment),
            remark: Set(deck_rate.remark),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert runtime rate");
    }
}

pub async fn insert_runtime_routing_profile_config(
    db: &DatabaseConnection,
    profile: RoutingProfileConfig,
) {
    let now = Utc::now();
    if let Some(existing) = routing_profile::Entity::find_by_id(profile.id)
        .one(db)
        .await
        .expect("load runtime routing profile")
    {
        let mut active: routing_profile::ActiveModel = existing.into();
        active.name = Set(profile.name.clone());
        active.description = Set(profile.description.clone());
        active.enable_retry_policy = Set(profile.enable_retry_policy);
        active.retry_codes = Set(profile.retry_codes.clone());
        active.max_failover_items = Set(profile.max_failover_items);
        active.no_trying_timeout_ms = Set(profile.no_trying_timeout_ms);
        active.updated_at = Set(now);
        active
            .update(db)
            .await
            .expect("update runtime routing profile");
    } else {
        routing_profile::ActiveModel {
            id: Set(profile.id),
            name: Set(profile.name.clone()),
            description: Set(profile.description.clone()),
            enable_retry_policy: Set(profile.enable_retry_policy),
            retry_codes: Set(profile.retry_codes.clone()),
            max_failover_items: Set(profile.max_failover_items),
            no_trying_timeout_ms: Set(profile.no_trying_timeout_ms),
            remark: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .expect("insert runtime routing profile");
    }

    routing_profile_item::Entity::delete_many()
        .filter(routing_profile_item::Column::ProfileId.eq(profile.id))
        .exec(db)
        .await
        .expect("clear runtime routing profile items");
    for item in profile.items {
        routing_profile_item::ActiveModel {
            profile_id: Set(profile.id),
            sip_trunk_id: Set(item.sip_trunk_id),
            is_active: Set(item.is_active),
            priority: Set(item.priority),
            weight: Set(item.weight),
            match_callee_prefix: Set(item.match_callee_prefix),
            match_caller_prefix: Set(item.match_caller_prefix),
            match_callee_country: Set(item.match_callee_country),
            match_caller_country: Set(item.match_caller_country),
            rewrite_callee: Set(item.rewrite_callee),
            rewrite_caller: Set(item.rewrite_caller),
            caller_number_pool: Set(item.caller_number_pool),
            caller_selection_policy: Set(item.caller_selection_policy),
            strip_digits: Set(item.strip_digits),
            prepend_digits: Set(item.prepend_digits),
            time_window_start: Set(item.time_window_start),
            time_window_end: Set(item.time_window_end),
            time_window_days: Set(item.time_window_days),
            time_window_timezone: Set(item.time_window_timezone),
            max_retries: Set(item.max_retries),
            remark: Set(None),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert runtime routing profile item");
    }
}

pub async fn load_runtime_routing_profiles(state: &WholesaleState, db: &DatabaseConnection) {
    let route_items = routing_profile_item::Entity::find()
        .all(db)
        .await
        .expect("load runtime routing profile items");
    let mut test_buy_deck_id = None;
    for item in route_items.into_iter().filter(|item| item.is_active) {
        let config = wholesale_trunk_config::Entity::find_by_id(item.sip_trunk_id)
            .one(db)
            .await
            .expect("load runtime wholesale trunk config");
        if config
            .as_ref()
            .and_then(|config| config.rate_deck_id)
            .is_some()
        {
            continue;
        }

        if test_buy_deck_id.is_none() {
            let deck = rate_deck::ActiveModel {
                name: Set("Test Buy Deck".to_string()),
                r#type: Set(rate_deck::RateDeckType::Buy),
                created_at: Set(Utc::now()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(db)
            .await
            .expect("insert test buy deck");
            rate::ActiveModel {
                deck_id: Set(deck.id),
                prefix: Set(String::new()),
                rate: Set(0.0),
                min_duration: Set(60),
                increment: Set(60),
                created_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(db)
            .await
            .expect("insert default test buy rate");
            test_buy_deck_id = Some(deck.id);
        }

        if let Some(config) = config {
            let mut active: wholesale_trunk_config::ActiveModel = config.into();
            active.rate_deck_id = Set(test_buy_deck_id);
            active.update(db).await.expect("update test trunk config");
        } else {
            wholesale_trunk_config::ActiveModel {
                sip_trunk_id: Set(item.sip_trunk_id),
                rate_deck_id: Set(test_buy_deck_id),
                ..Default::default()
            }
            .insert(db)
            .await
            .expect("insert test trunk config");
        }
    }
    state
        .reload_runtime(db)
        .await
        .expect("rebuild route runtime");
}
