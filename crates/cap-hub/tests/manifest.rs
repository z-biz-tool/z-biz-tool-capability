//! 产品清单的契约回归。
//!
//! 与 TS 侧 `z-biz-tool-shared/tests/deeplink.test.ts` 的产品清单部分一一对应。
//! 两边都验同一组不变量 —— 因为产品清单会被 box 读，也会被各 app 自己读，
//! 两边理解不一致会让「面板显示能传参、实际对方不认」。

use cap_hub::manifest::{is_product_id, ProductMeta, ProductRegistry};

fn meta(id: &str) -> ProductMeta {
    ProductMeta {
        id: id.to_string(),
        display_name: id.to_uppercase(),
        description: format!("{id} 测试元数据"),
        accepts_deep_link: true,
        actions: vec!["open".to_string()],
        keywords: vec![],
    }
}

#[test]
fn product_ids_have_no_duplicates() {
    let ids = cap_hub::manifest::PRODUCT_IDS;
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "产品 id 出现重复");
}

#[test]
fn invisible_repos_are_not_products() {
    // capability / shared / lead 没有用户可见的窗口，无法被 deep-link 唤起
    for invisible in ["capability", "shared", "lead"] {
        assert!(
            !is_product_id(invisible),
            "{invisible} 不该出现在产品 id 里"
        );
    }
}

#[test]
fn product_ids_match_the_family() {
    // 家族产品仓清单 —— 新增/移除产品时两边必须同步改
    let expected = [
        "aigen", "box", "cpu", "db", "file", "kb", "math", "mind", "music", "note", "pet", "sys",
        "terminal", "worker",
    ];
    let mut a = cap_hub::manifest::PRODUCT_IDS.to_vec();
    let mut b = expected.to_vec();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(
        a, b,
        "协议产品集合与家族产品仓不一致 —— 新增/移除产品时必须同步改 cap-hub 与 shared"
    );
}

#[test]
fn valid_meta_passes() {
    assert!(meta("db").validate().is_ok());
}

#[test]
fn invalid_meta_is_rejected() {
    let mut bad_id = meta("db");
    bad_id.id = "nosuch".to_string();
    assert!(bad_id.validate().is_err(), "非法 id 应被拒");

    let mut no_name = meta("db");
    no_name.display_name = String::new();
    assert!(no_name.validate().is_err(), "缺 displayName 应被拒");

    let mut no_desc = meta("db");
    no_desc.description = String::new();
    assert!(no_desc.validate().is_err(), "缺 description 应被拒");
}

#[test]
fn declaring_actions_without_deep_link_support_is_contradictory() {
    let mut m = meta("db");
    m.accepts_deep_link = false;
    let err = m.validate().unwrap_err();
    assert!(
        err.contains("acceptsDeepLink"),
        "原因应点名 acceptsDeepLink：{err}"
    );
}

#[test]
fn duplicate_ids_in_registry_are_rejected() {
    let reg = ProductRegistry {
        products: vec![meta("db"), meta("db")],
    };
    let err = reg.validate().unwrap_err();
    assert!(err.contains("重复"), "重复 id 应报错：{err}");
}

#[test]
fn registry_queries() {
    let mut file = meta("file");
    file.accepts_deep_link = false;
    file.actions = vec![];
    let mut note = meta("note");
    note.actions = vec!["open".to_string(), "create".to_string()];

    let reg = ProductRegistry {
        products: vec![meta("db"), file, note],
    };
    assert!(reg.validate().is_ok());

    let deep: Vec<&str> = reg
        .deep_link_products()
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(deep, vec!["db", "note"]);

    let by_create: Vec<&str> = reg
        .products_by_action("create")
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(by_create, vec!["note"]);

    assert_eq!(reg.find("db").map(|p| p.id.as_str()), Some("db"));
    assert!(reg.find("file").is_some());
}
