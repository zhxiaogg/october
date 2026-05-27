use crate::error::ToolCallError;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[async_trait]
pub trait Toolbox: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError>;
}
