//! HTTP file manager: list, download, and upload files from --shared-dir.

use axum::{
    body::Body,
    extract::{Multipart, Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
};
use std::sync::Arc;
use tokio_util::io::ReaderStream;

pub type AppStateRef = Arc<crate::transport::websocket::AppState>;

pub async fn list_handler(State(state): State<AppStateRef>) -> impl IntoResponse {
    let dir = match &state.shared_dir {
        Some(d) => d.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Html("<h1>Shared folder not configured</h1>".to_string()),
            )
        }
    };

    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>Cannot read directory</h1>".to_string()),
            )
        }
    };

    let mut rows = String::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().await.ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let human = human_bytes(size);
        rows.push_str(&format!(
            r#"<tr><td><a href="/files/{name}" download>{name}</a></td><td>{human}</td><td><a href="/files/{name}" download>↓</a></td></tr>"#
        ));
    }

    let html = format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>ddisplay — Shared Files</title>
<style>
body{{font-family:system-ui,sans-serif;max-width:800px;margin:40px auto;padding:0 20px;background:#1a1a2e;color:#e0e0e0}}
h1{{color:#7c82f5}}
table{{width:100%;border-collapse:collapse;margin-top:20px}}
th{{text-align:left;padding:8px;border-bottom:1px solid #444;color:#7c82f5}}
td{{padding:8px;border-bottom:1px solid #333}}
a{{color:#a0a8f8;text-decoration:none}}a:hover{{text-decoration:underline}}
#drop{{border:2px dashed #7c82f5;border-radius:8px;padding:40px;text-align:center;margin-top:24px;cursor:pointer;transition:background .2s}}
#drop.over{{background:rgba(124,130,245,.1)}}
#status{{margin-top:12px;min-height:1.5em;color:#a0f8c0}}
</style></head><body>
<h1>📁 Shared Files</h1>
<table><tr><th>Name</th><th>Size</th><th></th></tr>{rows}</table>
<div id="drop">Drop files here to upload, or <label for="fi" style="cursor:pointer;color:#a0a8f8;text-decoration:underline">browse</label>
<input id="fi" type="file" multiple style="display:none"></div>
<div id="status"></div>
<script>
const drop=document.getElementById('drop'),status=document.getElementById('status'),fi=document.getElementById('fi');
async function upload(files){{
  for(const f of files){{
    status.textContent='Uploading '+f.name+'...';
    const fd=new FormData();fd.append('file',f);
    const r=await fetch('/files/upload',{{method:'POST',body:fd}});
    if(r.ok){{status.textContent='✓ '+f.name+' uploaded';location.reload();}}
    else{{status.textContent='✗ Upload failed';}}
  }}
}}
drop.addEventListener('dragover',e=>{{e.preventDefault();drop.classList.add('over')}});
drop.addEventListener('dragleave',()=>drop.classList.remove('over'));
drop.addEventListener('drop',e=>{{e.preventDefault();drop.classList.remove('over');upload(e.dataTransfer.files)}});
fi.addEventListener('change',()=>upload(fi.files));
</script></body></html>"#
    );
    (StatusCode::OK, Html(html))
}

pub async fn download_handler(
    State(state): State<AppStateRef>,
    Path(name): Path<String>,
) -> Response {
    let dir = match &state.shared_dir {
        Some(d) => d.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    if name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let path = dir.join(&name);
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", name),
        )
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn upload_handler(
    State(state): State<AppStateRef>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let dir = match &state.shared_dir {
        Some(d) => d.clone(),
        None => return (StatusCode::NOT_FOUND, "Shared folder not configured"),
    };
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = match field.file_name() {
            Some(n) if !n.contains('/') && !n.contains('\\') && !n.starts_with('.') => {
                n.to_string()
            }
            _ => continue,
        };
        let data = match field.bytes().await {
            Ok(d) => d,
            Err(_) => continue,
        };
        let path = dir.join(&name);
        if tokio::fs::write(&path, &data).await.is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Write failed");
        }
        tracing::info!("[files] uploaded: {} ({} bytes)", name, data.len());
    }
    (StatusCode::OK, "OK")
}

fn human_bytes(b: u64) -> String {
    match b {
        0 => "0 B".into(),
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b if b < 1024 * 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        b => format!("{:.1} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)),
    }
}
