//! Async file transfer state: list, download, and upload via the server's HTTP file API.

use std::path::PathBuf;

#[derive(Clone, Debug, serde::Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
}

impl FileEntry {
    pub fn size_human(&self) -> String {
        match self.size {
            0 => "0 B".into(),
            b if b < 1024 => format!("{b} B"),
            b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
            b if b < 1024 * 1024 * 1024 => {
                format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
            }
            b => format!("{:.1} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)),
        }
    }
}

enum FileOp {
    FileList(Vec<FileEntry>),
    Status(String),
}

pub struct FileTransferState {
    pub files: Vec<FileEntry>,
    pub status: String,
    pub loading: bool,
    base_url: String,
    client: reqwest::Client,
    rt: tokio::runtime::Handle,
    tx: std::sync::mpsc::SyncSender<FileOp>,
    rx: std::sync::mpsc::Receiver<FileOp>,
}

impl FileTransferState {
    pub fn new(server: &str, rt: tokio::runtime::Handle) -> Self {
        let base_url = format!("http://{server}");
        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        let mut state = Self {
            files: vec![],
            status: String::new(),
            loading: false,
            base_url,
            client: reqwest::Client::new(),
            rt,
            tx,
            rx,
        };
        state.refresh();
        state
    }

    /// Drain async results and update state. Call once per frame.
    pub fn poll(&mut self) {
        while let Ok(op) = self.rx.try_recv() {
            match op {
                FileOp::FileList(files) => {
                    self.files = files;
                    self.loading = false;
                    self.status.clear();
                }
                FileOp::Status(s) => {
                    self.status = s;
                    self.loading = false;
                }
            }
        }
    }

    /// Reload the file list from the server.
    pub fn refresh(&mut self) {
        self.loading = true;
        let url = format!("{}/files/list", self.base_url);
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<Vec<FileEntry>>().await {
                        Ok(files) => {
                            let _ = tx.send(FileOp::FileList(files));
                        }
                        Err(e) => {
                            let _ = tx.send(FileOp::Status(format!("Parse error: {e}")));
                        }
                    }
                }
                Ok(resp) => {
                    let _ = tx.send(FileOp::Status(format!("Server error: {}", resp.status())));
                }
                Err(e) => {
                    let _ = tx.send(FileOp::Status(format!("Error: {e}")));
                }
            }
        });
    }

    /// Upload every file in `dir` to the server's shared folder (non-recursive).
    pub fn upload_dir(&mut self, dir: std::path::PathBuf) {
        let url = format!("{}/files/upload", self.base_url);
        let client = self.client.clone();
        let tx = self.tx.clone();
        let tx2 = tx.clone();
        let base_url = self.base_url.clone();
        let client2 = client.clone();
        self.rt.spawn(async move {
            let read = match tokio::fs::read_dir(&dir).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(FileOp::Status(format!("✗ Cannot read share-dir: {e}")));
                    return;
                }
            };

            let mut read = read;
            let mut uploaded = 0u32;
            let mut failed = 0u32;
            while let Ok(Some(entry)) = read.next_entry().await {
                let meta = match entry.metadata().await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !meta.is_file() {
                    continue;
                }
                let path = entry.path();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.is_empty() || name.starts_with('.') {
                    continue;
                }

                let bytes = match tokio::fs::read(&path).await {
                    Ok(b) => b,
                    Err(_) => { failed += 1; continue; }
                };
                let part = reqwest::multipart::Part::bytes(bytes).file_name(name);
                let form = reqwest::multipart::Form::new().part("file", part);
                match client.post(&url).multipart(form).send().await {
                    Ok(r) if r.status().is_success() => uploaded += 1,
                    _ => failed += 1,
                }
            }

            let msg = if failed == 0 {
                format!("✓ Shared {uploaded} file(s) from local folder")
            } else {
                format!("⚠ Shared {uploaded} file(s), {failed} failed")
            };
            let _ = tx.send(FileOp::Status(msg));

            // Refresh file list after bulk upload.
            let list_url = format!("{base_url}/files/list");
            if let Ok(resp) = client2.get(&list_url).send().await {
                if let Ok(files) = resp.json::<Vec<FileEntry>>().await {
                    let _ = tx2.send(FileOp::FileList(files));
                }
            }
        });
    }

    /// Download `name` to the user's Downloads folder.
    pub fn download(&mut self, name: String) {
        self.status = format!("Downloading {name}…");
        let url = format!("{}/files/{name}", self.base_url);
        let client = self.client.clone();
        let tx = self.tx.clone();
        let name_clone = name.clone();
        self.rt.spawn(async move {
            let result = async {
                let resp = client.get(&url).send().await?;
                let bytes = resp.bytes().await?;
                let dir = downloads_dir();
                let path = dir.join(&name_clone);
                tokio::fs::write(&path, &bytes).await?;
                Ok::<_, anyhow::Error>(path)
            }
            .await;

            let msg = match result {
                Ok(p) => format!("✓ Saved to {}", p.display()),
                Err(e) => format!("✗ Download failed: {e}"),
            };
            let _ = tx.send(FileOp::Status(msg));
        });
    }

    /// Upload a local file to the server's shared folder.
    pub fn upload(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        self.status = format!("Uploading {name}…");
        let url = format!("{}/files/upload", self.base_url);
        let client = self.client.clone();
        let tx = self.tx.clone();
        let tx2 = tx.clone();
        let base_url = self.base_url.clone();
        let client2 = client.clone();
        self.rt.spawn(async move {
            let result = async {
                let bytes = tokio::fs::read(&path).await?;
                let part = reqwest::multipart::Part::bytes(bytes).file_name(name.clone());
                let form = reqwest::multipart::Form::new().part("file", part);
                let resp = client.post(&url).multipart(form).send().await?;
                if resp.status().is_success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("Server returned {}", resp.status()))
                }
            }
            .await;

            let msg = match result {
                Ok(()) => format!("✓ {name} uploaded"),
                Err(e) => format!("✗ Upload failed: {e}"),
            };
            let _ = tx.send(FileOp::Status(msg));

            // Refresh the file list after upload.
            let list_url = format!("{base_url}/files/list");
            if let Ok(resp) = client2.get(&list_url).send().await {
                if let Ok(files) = resp.json::<Vec<FileEntry>>().await {
                    let _ = tx2.send(FileOp::FileList(files));
                }
            }
        });
    }
}

fn downloads_dir() -> PathBuf {
    if let Ok(home) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(home).join("Downloads");
        if p.exists() {
            return p;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join("Downloads");
        if p.exists() {
            return p;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
