use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub percentage: u8,
    pub message: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy)]
pub enum ProgressStep {
    Started,
    DetectingPlatforms,
    ValidatingPlatforms,
    CreatingWorkDir,
    WritingBinaries,
    CreatingLoader,
    ConvertingToObjects,
    CompilingLoader,
    Linking,
    Complete,
}

impl ProgressStep {
    pub fn percentage(&self) -> u8 {
        match self {
            ProgressStep::Started => 0,
            ProgressStep::DetectingPlatforms => 10,
            ProgressStep::ValidatingPlatforms => 20,
            ProgressStep::CreatingWorkDir => 25,
            ProgressStep::WritingBinaries => 35,
            ProgressStep::CreatingLoader => 45,
            ProgressStep::ConvertingToObjects => 60,
            ProgressStep::CompilingLoader => 75,
            ProgressStep::Linking => 85,
            ProgressStep::Complete => 100,
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            ProgressStep::Started => "Starting merge operation",
            ProgressStep::DetectingPlatforms => "Detecting binary platforms",
            ProgressStep::ValidatingPlatforms => "Validating platform compatibility",
            ProgressStep::CreatingWorkDir => "Creating working directory",
            ProgressStep::WritingBinaries => "Writing binary files",
            ProgressStep::CreatingLoader => "Creating loader stub",
            ProgressStep::ConvertingToObjects => "Converting binaries to object files",
            ProgressStep::CompilingLoader => "Compiling loader",
            ProgressStep::Linking => "Linking everything together",
            ProgressStep::Complete => "Merge complete",
        }
    }
}

pub struct ProgressTracker {
    redis_client: redis::Client,
    task_id: String,
}

impl ProgressTracker {
    pub fn new(redis_url: &str, task_id: String) -> Result<Self> {
        let redis_client = redis::Client::open(redis_url)?;
        Ok(Self { redis_client, task_id })
    }

    pub async fn update(&self, step: ProgressStep) -> Result<()> {
        let mut conn = self.redis_client.get_async_connection().await?;
        
        let progress = Progress {
            percentage: step.percentage(),
            message: step.message().to_string(),
            updated_at: chrono::Utc::now().timestamp(),
        };
        
        let channel = format!("progress:{}", self.task_id);
        let key = format!("progress_cache:{}", self.task_id);
        let value = serde_json::to_string(&progress)?;
        
        // 1. Publish to channel (for real-time subscribers)
        let _: () = conn.publish(&channel, &value).await?;
        
        // 2. Also cache in Redis (for GET fallback)
        let _: () = conn.set_ex(&key, &value, 3600).await?;
        
        log::info!("Progress update: {}% - {}", progress.percentage, progress.message);
        
        Ok(())
    }

    pub async fn report_io_progress(&self, bytes_written: u64, total_size: u64, base_step: ProgressStep) -> Result<()> {
        let mut conn = self.redis_client.get_async_connection().await?;
        
        let base_percentage = base_step.percentage() as f64;
        let next_percentage = ProgressStep::CreatingLoader.percentage() as f64;
        let range = next_percentage - base_percentage;
        
        let io_percentage = (bytes_written as f64 / total_size as f64) * range;
        let final_percentage = (base_percentage + io_percentage).min(next_percentage) as u8;

        let progress = Progress {
            percentage: final_percentage,
            message: "Writing binary data...".to_string(),
            updated_at: chrono::Utc::now().timestamp(),
        };

        let channel = format!("progress:{}", self.task_id);
        let value = serde_json::to_string(&progress)?;
        let _: () = conn.publish(&channel, &value).await?;

        Ok(())
    }

    pub async fn get(redis_url: &str, task_id: &str) -> Result<Option<Progress>> {
        let client = redis::Client::open(redis_url)?;
        let mut conn = client.get_async_connection().await?;
        
        let key = format!("progress_cache:{}", task_id);
        let value: Option<String> = conn.get(&key).await?;
        
        match value {
            Some(v) => Ok(Some(serde_json::from_str(&v)?)),
            None => Ok(None),
        }
    }

    pub async fn delete(redis_url: &str, task_id: &str) -> Result<()> {
        let client = redis::Client::open(redis_url)?;
        let mut conn = client.get_async_connection().await?;
        
        let key = format!("progress_cache:{}", task_id);
        let _: () = conn.del(&key).await?;
        
        Ok(())
    }
    
    pub async fn publish_complete(redis_url: &str, task_id: &str, binary_id: Option<String>, error: Option<String>, wrapped_size: Option<u64>) -> Result<()> {
        let client = redis::Client::open(redis_url)?;
        let mut conn = client.get_async_connection().await?;
        
        let channel = format!("progress:{}", task_id);
        
        // Build download_url if binary_id is present and no error
        let download_url = if error.is_none() {
            binary_id.as_ref().map(|id| format!("/download/{}", id))
        } else {
            None
        };
        
        let message = serde_json::json!({
            "percentage": 100,
            "message": if error.is_some() { "Failed" } else { "Complete" },
            "updated_at": chrono::Utc::now().timestamp(),
            "complete": true,
            "binary_id": binary_id,
            "download_url": download_url,
            "error": error,
            "wrapped_size": wrapped_size,
        });
        
        let _: () = conn.publish(&channel, serde_json::to_string(&message)?).await?;
        
        Ok(())
    }
}
