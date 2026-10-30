//! 家族产品清单 —— 每个 app 声明自己支持什么。
//!
//! # 谁维护什么
//!
//! **协议格式**（本 crate）是全家族共用的，改一次全家生效。
//!
//! **能力清单**（本模块）是**会过期的数据** —— 显示名会改、
//! 支持深链的状态会变。所以它带着「每条记录都有主人」的约束：
//! 每个 app 在自己的仓里维护自己那一条（`accepts_deep_link` 这类事实
//! 只有那个 app 知道），本模块提供**类型 + 校验 + 汇总**，
//! 不内置任何一条真值。
//!
//! # 为什么 capability / shared / lead 不在清单里
//!
//! 那三个仓没有用户可见的窗口，无法被 deep-link 唤起。
//! 把它们列进清单会让 box 的家族面板显示出三个打不开的卡片。

use serde::{Deserialize, Serialize};

/// 家族产品 id。
///
/// 与 bundle id / 数据目录命名保持一致（`z-biz-tool-db` → `db`），
/// 减少用户与开发者的心智负担。
pub const PRODUCT_IDS: &[&str] = &[
    "aigen", "box", "cpu", "db", "file", "kb", "math", "mind", "music", "note", "pet", "sys",
    "terminal", "worker",
];

/// 判断一个字符串是不是合法的产品 id。
pub fn is_product_id(value: &str) -> bool {
    PRODUCT_IDS.contains(&value)
}

/// 一个产品的元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductMeta {
    /// 产品 id。
    pub id: String,
    /// 显示名（中文优先）。
    pub display_name: String,
    /// 一句话描述，给家族面板用。
    pub description: String,
    /// 该产品是否接受 `zbiz://` 深链。
    ///
    /// false 表示「这个 app 还没接协议」—— box 仍可以启动它，
    /// 但不该带参数（带了对方也不认）。
    #[serde(default)]
    pub accepts_deep_link: bool,
    /// 该产品支持的动作。空数组 = 只能被唤起，不能接收参数。
    #[serde(default)]
    pub actions: Vec<String>,
    /// 可选的搜索关键词，给 box 的 ⌘K 用。
    #[serde(default)]
    pub keywords: Vec<String>,
}

impl ProductMeta {
    /// 校验元数据是否自洽。
    ///
    /// 挡的是这条链：「清单写错 → box 家族面板显示出这个 app →
    /// 用户点下去发现参数被丢了」。清单错误不会有编译错误，
    /// 只会在用户点下去之后才暴露，而那时用户看到的是「点了没反应」。
    pub fn validate(&self) -> Result<(), String> {
        if !is_product_id(&self.id) {
            return Err(format!("产品 id 非法：{}", self.id));
        }
        if self.display_name.is_empty() {
            return Err(format!("产品 {} 缺少 displayName", self.id));
        }
        if self.description.is_empty() {
            return Err(format!("产品 {} 缺少 description", self.id));
        }
        // 关键自洽性：声明了动作却说自己不接深链，是自相矛盾的。
        if !self.actions.is_empty() && !self.accepts_deep_link {
            return Err(format!(
                "产品 {} 声明了 actions 却未开启 acceptsDeepLink",
                self.id
            ));
        }
        Ok(())
    }
}

/// 一组产品元数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductRegistry {
    pub products: Vec<ProductMeta>,
}

impl ProductRegistry {
    /// 校验整份清单。
    ///
    /// **重复 id 直接报错**，不静默取最后一份 —— 两个仓同时声明 `db`
    /// 意味着有人改了协议却没同步，这在协议层必须炸出来。
    pub fn validate(&self) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::new();
        for p in &self.products {
            p.validate()?;
            if seen.contains(&p.id.as_str()) {
                return Err(format!("产品元数据重复声明 id：{}", p.id));
            }
            seen.push(&p.id);
        }
        Ok(())
    }

    /// 按 id 找一个产品。
    pub fn find(&self, id: &str) -> Option<&ProductMeta> {
        self.products.iter().find(|p| p.id == id)
    }

    /// 列出「接受深链」的产品。
    pub fn deep_link_products(&self) -> Vec<&ProductMeta> {
        self.products
            .iter()
            .filter(|p| p.accepts_deep_link)
            .collect()
    }

    /// 列出「接受深链且支持指定动作」的产品。
    pub fn products_by_action(&self, action: &str) -> Vec<&ProductMeta> {
        self.deep_link_products()
            .into_iter()
            .filter(|p| p.actions.iter().any(|a| a == action))
            .collect()
    }
}
