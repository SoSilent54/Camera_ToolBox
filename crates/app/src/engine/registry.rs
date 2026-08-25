//! 节点注册表：kind → 工厂的动态分发。

use std::collections::HashMap;

use super::node::NodeFactory;
use super::spec::NodeKindId;

/// 按 kind 注册节点工厂。新增节点只需 `register` 一个工厂，无需改引擎。
#[derive(Default)]
pub struct NodeRegistry {
    factories: HashMap<NodeKindId, Box<dyn NodeFactory>>,
}

impl NodeRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个工厂；返回被替换的旧工厂（若 kind 已存在）。
    pub fn register(&mut self, factory: Box<dyn NodeFactory>) -> Option<Box<dyn NodeFactory>> {
        self.factories.insert(factory.kind().to_owned(), factory)
    }

    /// 按 kind 查找工厂。
    #[must_use]
    pub fn get(&self, kind: &str) -> Option<&dyn NodeFactory> {
        self.factories.get(kind).map(Box::as_ref)
    }

    /// 已注册的全部 kind。
    #[must_use]
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.factories.keys().map(String::as_str)
    }
}
