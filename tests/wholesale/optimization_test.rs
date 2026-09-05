use rustpbx::addons::wholesale::data::{RateConfig, RateMatcher, RoutingProfileItemConfig};
use rustpbx::addons::wholesale::matching;
use rustpbx::addons::wholesale::trie::PrefixTrie;

#[test]
fn test_rate_trie_matching() {
    let mut trie = PrefixTrie::new();
    trie.insert(
        "1",
        RateConfig {
                        prefix: "1".to_string(),
            match_caller_prefix: None,
            rate: 0.01,
            min_duration: 60,
            increment: 60,
            remark: None,
        },
    );
    trie.insert(
        "1212",
        RateConfig {
                        prefix: "1212".to_string(),
            match_caller_prefix: None,
            rate: 0.02,
            min_duration: 60,
            increment: 60,
            remark: None,
        },
    );
    trie.insert(
        "86",
        RateConfig {
                        prefix: "86".to_string(),
            match_caller_prefix: None,
            rate: 0.05,
            min_duration: 60,
            increment: 60,
            remark: None,
        },
    );

    assert_eq!(
        trie.find_longest_prefix("1212345678").unwrap().prefix,
        "1212"
    );
    assert_eq!(trie.find_longest_prefix("1312345678").unwrap().prefix, "1");
    assert_eq!(
        trie.find_longest_prefix("86138000000").unwrap().prefix,
        "86"
    );
    assert!(trie.find_longest_prefix("4412345678").is_none());
}

#[test]
fn test_routing_trie_matching() {
    let mut trie = PrefixTrie::new();

    let item1 = RoutingProfileItemConfig {
        id: 1,
        sip_trunk_id: 101,
        priority: 1,
        weight: 100,
        match_callee_prefix: Some("1".to_string()),
        ..Default::default()
    };
    let item2 = RoutingProfileItemConfig {
        id: 2,
        sip_trunk_id: 102,
        priority: 2,
        weight: 100,
        match_callee_prefix: Some("1".to_string()),
        ..Default::default()
    };
    let item3 = RoutingProfileItemConfig {
        id: 3,
        sip_trunk_id: 103,
        priority: 1,
        weight: 100,
        match_callee_prefix: Some("1212".to_string()),
        ..Default::default()
    };

    trie.insert("1", vec![item1.clone(), item2.clone()]);
    trie.insert("1212", vec![item3.clone()]);

    let matches = trie.find_all_matching_prefixes("1212345678");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0][0].id, 1);
    assert_eq!(matches[0][1].id, 2);
    assert_eq!(matches[1][0].id, 3);
}

#[test]
fn test_rewrite_rule_try_from() {
    let pattern = "s/^86(.*)$/0$1/";
    let rule = matching::RewriteRule::try_from(pattern).expect("compile rewrite rule");

    let result = rule.replace("8613800138000");
    assert_eq!(result, "013800138000");

    let pattern2 = "s/^(\\d{2})(\\d+)$/${1}99${2}/";
    let rule = matching::RewriteRule::try_from(pattern2).expect("compile rewrite rule");
    let result2 = rule.replace("12345");
    assert_eq!(result2, "1299345");
}

#[test]
fn test_wholesale_state_load_and_find() {
    let rates = RateMatcher::from(vec![
        RateConfig {
                        prefix: "1".to_string(),
            match_caller_prefix: None,
            rate: 0.1,
            min_duration: 1,
            increment: 1,
            remark: None,
        },
        RateConfig {
                        prefix: "123".to_string(),
            match_caller_prefix: None,
            rate: 0.2,
            min_duration: 1,
            increment: 1,
            remark: None,
        },
    ]);

    let rate = rates.find_best_rate("123456", None).unwrap();
    assert_eq!(rate.prefix, "123");
    assert_eq!(rate.rate, 0.2);

    let rate2 = rates.find_best_rate("144444", None).unwrap();
    assert_eq!(rate2.prefix, "1");
}
