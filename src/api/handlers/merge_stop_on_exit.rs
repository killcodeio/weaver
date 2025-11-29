use actix_web::{web, HttpResponse, Error};
use actix_multipart::form::{tempfile::TempFile, MultipartForm};
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;
use chrono::{Utc, Duration};

use crate::models::{
    response::{MergeResponse, ErrorResponse},
    binary::StoredBinary,
};
use crate::core::progress::{ProgressTracker, ProgressStep};
use crate::core::binary::BinaryInfo;
use crate::config::Config;

#[derive(Debug, MultipartForm)]
pub struct StopOnExitForm {
    #[multipart(limit = "200MB")]
    pub base_binary: TempFile,
    #[multipart(limit = "200MB")]
    pub overload_binary: TempFile,
    #[multipart(rename = "output_name")]
    pub output_name: Option<actix_multipart::form::text::Text<String>>,
    #[multipart(rename = "task_id")]
    pub task_id: Option<actix_multipart::form::text::Text<String>>,
}

/// New merge endpoint that stops overload when base exits
/// POST /merge/stop-on-exit
pub async fn merge_stop_on_exit(
    MultipartForm(form): MultipartForm<StopOnExitForm>,
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

    log::info!("🔪 Merging binaries with STOP-ON-EXIT mode");
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
            error: error_msg,
            details: None,
        }));
    }

    // Create temp directory
    std::fs::create_dir_all(&config.temp_dir)
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    let work_dir = tempfile::TempDir::new_in(&config.temp_dir)
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    let work_path = work_dir.path();
    
    let task_id_str = task_id.as_deref().unwrap_or("");
    
    // Perform the merge with stop-on-exit logic (parent monitors base and kills overload)
    match crate::core::merger::merge_stop_on_exit(
        &base_data,
        &overload_data,
        work_path,
        &base_info,
        task_id_str,
    ).await {
        Ok(merged_path) => {
            let binary_id = Uuid::new_v4().to_string();
            
            // Copy to permanent location with UUID
            let final_path = std::path::PathBuf::from(&config.temp_dir)
                .join(format!("merged_{}.bin", binary_id));
            
            std::fs::copy(&merged_path, &final_path)
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
            
            let metadata = std::fs::metadata(&final_path).unwrap();
            let size = metadata.len();
            
            let now = Utc::now();
            let expires_at = now + Duration::seconds(config.binary_ttl);
            
            let stored = StoredBinary {
                id: binary_id.clone(),
                path: final_path.to_string_lossy().to_string(),
                size,
                created_at: now,
                expires_at,
            };
            
            // Store the binary
            {
                let mut store = binary_store.lock().unwrap();
                store.insert(binary_id.clone(), stored);
            }
            
            log::info!("✅ Stop-on-exit merge successful! Binary ID: {}, Size: {} bytes", binary_id, size);
            
            // Publish completion to Redis
            if let Some(ref tid) = task_id {
                let _ = ProgressTracker::publish_complete(
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
            log::error!("❌ Stop-on-exit merge failed: {}", e);
            
            // Publish error to Redis
            if let Some(ref tid) = task_id {
                let _ = ProgressTracker::publish_complete(
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
