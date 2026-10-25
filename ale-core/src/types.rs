use serde::{Deserialize, Serialize};

/// 引擎状态
#[derive(Debug, Serialize, Deserialize)]
pub struct EngineStatus {
    pub cloud_ready: bool,
}
