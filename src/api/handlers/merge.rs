use actix_web::{web, HttpResponse, Error};
use actix_multipart::form::{tempfile::TempFile, MultipartForm};
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;
use chrono::{Utc, Duration};

use crate::models::{
    request::MergeMode,
    response::{MergeResponse, ErrorResponse},
    binary::StoredBinary,
};
use crate::core;
use crate::core::progress::{ProgressTracker, ProgressStep};
use crate::config::Config;

#[derive(Debug, MultipartForm)]
pub struct MergeForm {
    #[multipart(limit = "200MB")]
    pub base_binary: TempFile,
    #[multipart(limit = "200MB")]
    pub overload_binary: TempFile,
    #[multipart(rename = "mode")]
    pub mode: Option<actix_multipart::form::text::Text<String>>,
    #[multipart(rename = "sync")]
    pub sync: Option<actix_multipart::form::text::Text<String>>,
    #[multipart(rename = "output_name")]
    pub output_name: Option<actix_multipart::form::text::Text<String>>,
    #[multipart(rename = "task_id")]
    pub task_id: Option<actix_multipart::form::text::Text<String>>,
}

pub async fn merge_binaries(
    MultipartForm(form): MultipartForm<MergeForm>,
    binary_store: web::Data<Mutex<HashMap<String, StoredBinary>>>,
    config: web::Data<Config>,
) -> Result<HttpResponse, Error> {
    // Read binary data from temp files
    let base_data = std::fs::read(&form.base_binary.file.path())
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    let overload_data = std::fs::read(&form.overload_binary.file.path())
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    
    // Parse parameters
    let mode = form.mode
        .as_ref()
        .and_then(|t| match t.as_str() {
            "after" => Some(MergeMode::After),
            "before" => Some(MergeMode::Before),
            _ => None
        })
        .unwrap_or(MergeMode::Before);
    
    let sync = form.sync
        .as_ref()
        .map(|t| t.as_str() == "true")
        .unwrap_or(false);
    
    let _output_name = form.output_name.as_ref().map(|t| t.to_string());
    
    // Validate file sizes
    if base_data.len() > config.max_file_size {
        return Ok(HttpResponse::BadRequest().json(ErrorResponse {
            error: "Base binary too large".to_string(),
            details: Some(format!("Max size: {} bytes", config.max_file_size)),
        }));
    }
    
    if overload_data.len() > config.max_file_size {
        return Ok(HttpResponse::BadRequest().json(ErrorResponse {
            error: "Overload binary too large".to_string(),
            details: Some(format!("Max size: {} bytes", config.max_file_size)),
        }));
    }

    log::info!("Merging binaries: mode={:?}, sync={}", mode, sync);
    log::info!("Base size: {} bytes, Overload size: {} bytes", base_data.len(), overload_data.len());

    // Get task_id for progress tracking
    let task_id = form.task_id.as_ref().map(|t| t.to_string());
    
    // Initialize progress tracker if task_id provided
    let progress_tracker = if let Some(ref tid) = task_id {
        match ProgressTracker::new(&config.redis_url, tid.clone()) {
            Ok(tracker) => {
                let _ = tracker.update(ProgressStep::Started).await;
                Some(tracker)
            }
            Err(e) => {
                log::warn!("Failed to create progress tracker: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Perform the merge
    let task_id_str = task_id.as_deref().unwrap_or("");
    match core::merge_binaries(&base_data, &overload_data, mode, sync, &config.temp_dir, task_id_str, &config.redis_url).await {
        Ok(merged_path) => {
            let binary_id = Uuid::new_v4().to_string();
            let metadata = std::fs::metadata(&merged_path).unwrap();
            let size = metadata.len();
            
            let now = Utc::now();
            let expires_at = now + Duration::seconds(config.binary_ttl);
            
            let stored = StoredBinary {
                id: binary_id.clone(),
                path: merged_path,
                size,
                created_at: now,
                expires_at,
            };
            
            // Store the binary
            {
                let mut store = binary_store.lock().unwrap();
                store.insert(binary_id.clone(), stored);
            }
            
            log::info!("✅ Merge successful! Binary ID: {}, Size: {} bytes", binary_id, size);
            
            // Publish completion to Redis
            if let Some(ref tid) = task_id {
                let _ = crate::core::progress::ProgressTracker::publish_complete(
                    &config.redis_url,
                    tid,
                    Some(binary_id.clone()),
                    None,
                    Some(size),
                ).await;
            }
            
            Ok(HttpResponse::Ok().json(MergeResponse {
                success: true,
                binary_id: binary_id.clone(),
                size,
                download_url: format!("/download/{}", binary_id),
                expires_at,
                error: None,
            }))
        }
        Err(e) => {
            log::error!("❌ Merge failed: {}", e);
            
            // Publish error to Redis
            if let Some(ref tid) = task_id {
                let _ = crate::core::progress::ProgressTracker::publish_complete(
                    &config.redis_url,
                    tid,
                    None,
                    Some(e.to_string()),
                    None,
                ).await;
            }
            
            Ok(HttpResponse::InternalServerError().json(MergeResponse {
                success: false,
                binary_id: String::new(),
                size: 0,
                download_url: String::new(),
                expires_at: Utc::now(),
                error: Some(e.to_string()),
            }))
        }
    }
}
