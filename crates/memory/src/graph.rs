use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySymbol {
    pub name: String,
    pub kind: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNeighbor {
    pub kind: String,
    pub key: String,
    pub label: String,
    pub relation: String,
}
