use super::*;
pub const MODE_CREATE: u32 = 0;
pub const MODE_READ_WRITE: u32 = 1;
pub const MODE_READ_ONLY: u32 = 2;

pub fn open_request(path: &[u8], mode: u32) -> ZeGraphOpenRequest {
    ZeGraphOpenRequest {
        abi_size: size_of::<ZeGraphOpenRequest>() as u32,
        abi_reserved: 0,
        path: ZeGraphBytes {
            data: path.as_ptr(),
            count: path.len(),
        },
        mode,
        tokenizer_profile: 0,
        document_tower: std::ptr::null(),
        reader_drain_timeout_ms: 250,
        max_resident_bytes: 256 << 20,
        control: std::ptr::null(),
    }
}
pub fn graph_open(path: &Path, mode: u32) -> (ZeErrorCode, ZeGraphHandle) {
    let path = path.to_str().expect("UTF-8 path").as_bytes();
    let mut handle = ZeGraphHandle { token: 0 };
    (
        ze_graph_open(&open_request(path, mode), &mut handle),
        handle,
    )
}
pub struct GraphTestStore {
    pub handle: ZeGraphHandle,
    pub path: PathBuf,
    dir: Option<TempDir>,
}
impl GraphTestStore {
    pub fn create() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph");
        let (code, handle) = graph_open(&path, MODE_CREATE);
        assert_eq!(code, ZeErrorCode::ZeOk);
        Self {
            handle,
            path,
            dir: Some(dir),
        }
    }
    pub fn close(&mut self) -> ZeErrorCode {
        let code = ze_graph_close(self.handle);
        self.handle.token = 0;
        code
    }
}
impl Drop for GraphTestStore {
    fn drop(&mut self) {
        if self.handle.token != 0 {
            let _ = ze_graph_close(self.handle);
        }
        let _ = self.dir.take();
    }
}
pub fn last_error(token: u64) -> String {
    let mut length = 0;
    assert_eq!(
        ze_last_error_message(token, std::ptr::null_mut(), 0, &mut length),
        ZeErrorCode::ZeOk
    );
    let mut bytes = vec![0u8; length + 1];
    assert_eq!(
        ze_last_error_message(token, bytes.as_mut_ptr().cast(), bytes.len(), &mut length),
        ZeErrorCode::ZeOk
    );
    bytes.truncate(length);
    String::from_utf8(bytes).unwrap()
}
