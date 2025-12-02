use actix_web::{web, HttpResponse, Error};
use actix_multipart::form::{tempfile::TempFile, MultipartForm};
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

use crate::models::{
    response::{MergeResponse, ErrorResponse},
    binary::StoredBinary,
};
use crate::core;
use crate::core::progress::{ProgressTracker, ProgressStep};
use crate::core::binary::BinaryInfo;
use crate::config::Config;

#[derive(Debug, MultipartForm)]
pub struct MergeV2Form {
    #[multipart(limit = "200MB")]
    pub base_binary: TempFile,
    #[multipart(limit = "200MB")]
    pub overload_binary: TempFile,
    #[multipart(rename = "output_name")]
    pub output_name: Option<actix_multipart::form::text::Text<String>>,
    #[multipart(rename = "task_id")]
    pub task_id: Option<actix_multipart::form::text::Text<String>>,
    
    // V2 Config Options
    #[multipart(rename = "grace_period")]
    pub grace_period: Option<actix_multipart::form::text::Text<u32>>,
    #[multipart(rename = "sync_mode")]
    pub sync_mode: Option<actix_multipart::form::text::Text<bool>>,
    #[multipart(rename = "network_failure_kill_count")]
    pub network_failure_kill_count: Option<actix_multipart::form::text::Text<u32>>,
}

/// V2 merge endpoint with advanced health monitoring
/// POST /merge/v2/stop-on-exit
pub async fn merge_v2_stop_on_exit(
    MultipartForm(form): MultipartForm<MergeV2Form>,
    binary_store: web::Data<Mutex<HashMap<String, StoredBinary>>>,
    config: web::Data<Config>,
) -> Result<HttpResponse, Error> {
    // Read binary data from temp files
    let base_data = std::fs::read(&form.base_binary.file.path())
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    let overload_data = std::fs::read(&form.overload_binary.file.path())
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    
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

    // Extract V2 config options
    let grace_period = form.grace_period.as_ref().map(|t| **t).unwrap_or(0);
    let sync_mode = form.sync_mode.as_ref().map(|t| **t).unwrap_or(false);
    let network_failure_kill_count = form.network_failure_kill_count.as_ref().map(|t| **t).unwrap_or(0);

    log::info!("🔪 V2 Merging binaries with advanced health monitoring");
    log::info!("Base size: {} bytes, Overload size: {} bytes", base_data.len(), overload_data.len());
    log::info!("Config: grace_period={}s, sync_mode={}, network_failure_kill_count={}", 
               grace_period, sync_mode, network_failure_kill_count);

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

    // Detect base binary info
    let base_info = BinaryInfo::detect(&base_data);
    
    log::info!("🔍 Detected base binary: {}", base_info.description());
    
    // Validate compatibility
    let overload_info = BinaryInfo::detect(&overload_data);
    if !base_info.is_compatible_with(&overload_info) {
        let error_msg = format!(
            "❌ Binary mismatch! Base is {} but overload is {}",
            base_info.description(),
            overload_info.description()
        );
        log::error!("{}", error_msg);
        
        if let Some(ref tid) = task_id {
            let _ = ProgressTracker::publish_complete(
                &config.redis_url,
                tid,
                None,
                Some(error_msg.clone()),
                None,
            ).await;
        }
        
        return Ok(HttpResponse::BadRequest().json(ErrorResponse {
            error: "Binary architecture mismatch".to_string(),
            details: Some(error_msg),
        }));
    }

    // Report: Merging binaries
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::WritingBinaries).await;
    }

    // Create work directory
    let work_dir = format!("/tmp/weaver/merge_{}", Uuid::new_v4());
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    let work_dir_path = std::path::Path::new(&work_dir);

    // Perform V2 merge with health monitoring
    let merge_result = core::merger::merge_v2_stop_on_exit(
        &base_data,
        &overload_data,
        work_dir_path,
        &base_info,
        task_id.as_deref().unwrap_or(""),
        &config.redis_url,
        grace_period,
        sync_mode,
        network_failure_kill_count,
    ).await;

    match merge_result {
        Ok(merged_path) => {
            let merged_id = Uuid::new_v4().to_string();
            
            // Copy to permanent location with UUID
            let final_path = std::path::PathBuf::from(&config.temp_dir)
                .join(format!("merged_{}.bin", merged_id));
            
            std::fs::copy(&merged_path, &final_path)
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
            
            let metadata = std::fs::metadata(&final_path)
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
            let size = metadata.len();
            
            let now = chrono::Utc::now();
            let expires_at = now + chrono::Duration::seconds(config.binary_ttl);
            
            // Store in memory
            let stored = StoredBinary {
                id: merged_id.clone(),
                path: final_path.to_string_lossy().to_string(),
                size,
                created_at: now,
                expires_at: expires_at.clone(),
            };
            
            binary_store.lock().unwrap().insert(merged_id.clone(), stored);
            
            log::info!("✅ Stored merged binary at: {}", final_path.display());

            // Cleanup work directory
            let _ = std::fs::remove_dir_all(&work_dir);

            // Report completion
            if let Some(ref tid) = task_id {
                let _ = ProgressTracker::publish_complete(
                    &config.redis_url,
                    tid,
                    Some(merged_id.clone()),
                    None,
                    Some(size),
                ).await;
            }

            log::info!("✅ V2 merge completed: {} bytes", size);

            Ok(HttpResponse::Ok().json(MergeResponse {
                success: true,
                binary_id: merged_id.clone(),
                size,
                download_url: format!("/download/{}", merged_id),
                expires_at,
                error: None,
            }))
        }
        Err(e) => {
            let error_msg = format!("Merge failed: {}", e);
            log::error!("❌ {}", error_msg);

            if let Some(ref tid) = task_id {
                let _ = ProgressTracker::publish_complete(
                    &config.redis_url,
                    tid,
                    None,
                    Some(error_msg.clone()),
                    None,
                ).await;
            }

            // Cleanup
            let _ = std::fs::remove_dir_all(&work_dir);

            Ok(HttpResponse::InternalServerError().json(ErrorResponse {
                error: "Merge failed".to_string(),
                details: Some(error_msg),
            }))
        }
    }
}
